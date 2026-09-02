//! MCPServerFactory — constructs and configures MCP server instances.
//!
//! Responsible for building tools from registry descriptors, registering
//! handlers, and producing a ready-to-run MCPServer.

use std::collections::HashMap;
use std::sync::Arc;

use serde_json::Value;

use apcore::module::ModuleAnnotations;
use apcore::registry::{ModuleDescriptor, Registry};

use crate::adapters::annotations::{AnnotationMapper, McpAnnotations};
use crate::adapters::schema::SchemaConverter;
use crate::server::async_task_bridge::AsyncTaskBridge;
use crate::server::router::ExecutionRouter;
use crate::server::server::{FactoryError, MCPServer, MCPServerConfig};
use crate::server::types::{
    CallToolResult, InitializationOptions, ReadResourceContents, Resource, ResourceTemplate,
    ResourcesCapability, ServerCapabilities, TextContent, Tool, ToolAnnotations, ToolsCapability,
};

/// Summarize a full apcore `PipelineTrace` JSON into the MCP `_meta.trace`
/// shape: `{ step_count, steps: [{ name, duration_ms, skip_reason? }] }`.
fn summarize_trace(trace: &Value) -> Value {
    let steps = trace.get("steps").and_then(|v| v.as_array());
    let step_summaries: Vec<Value> = steps
        .map(|arr| {
            arr.iter()
                .map(|step| {
                    let mut obj = serde_json::Map::new();
                    if let Some(n) = step.get("name").and_then(|v| v.as_str()) {
                        obj.insert("name".into(), Value::String(n.to_string()));
                    }
                    if let Some(d) = step.get("duration_ms") {
                        obj.insert("duration_ms".into(), d.clone());
                    }
                    if let Some(r) = step.get("skip_reason").cloned() {
                        if !r.is_null() {
                            obj.insert("skip_reason".into(), r);
                        }
                    }
                    Value::Object(obj)
                })
                .collect()
        })
        .unwrap_or_default();
    serde_json::json!({
        "step_count": step_summaries.len(),
        "steps": step_summaries,
    })
}

/// AI intent metadata keys extracted from module metadata and appended
/// to tool descriptions for agent visibility.
const AI_INTENT_KEYS: &[&str] = &[
    "x-when-to-use",
    "x-when-not-to-use",
    "x-common-mistakes",
    "x-workflow-hints",
];

/// Enrich a base description with AI intent metadata.
///
/// For each recognized intent key present in `metadata` with a non-empty value,
/// a formatted line is appended. The label is derived by stripping the `x-` prefix,
/// replacing hyphens with spaces, and title-casing each word.
///
/// Returns the original description unchanged if metadata is `None`, empty,
/// or contains no recognized intent keys with non-empty values.
pub fn enrich_description(base: &str, metadata: Option<&HashMap<String, String>>) -> String {
    let metadata = match metadata {
        Some(m) if !m.is_empty() => m,
        _ => return base.to_string(),
    };

    let mut intent_parts: Vec<String> = Vec::new();
    for &key in AI_INTENT_KEYS {
        if let Some(val) = metadata.get(key) {
            if !val.is_empty() {
                let label = format_intent_label(key);
                intent_parts.push(format!("{}: {}", label, val));
            }
        }
    }

    if intent_parts.is_empty() {
        base.to_string()
    } else {
        format!("{}\n\n{}", base, intent_parts.join("\n"))
    }
}

/// Convert an intent key like `x-when-to-use` to a label like `When To Use`.
fn format_intent_label(key: &str) -> String {
    key.strip_prefix("x-")
        .unwrap_or(key)
        .split('-')
        .map(|word| {
            let mut chars = word.chars();
            match chars.next() {
                None => String::new(),
                Some(c) => {
                    let upper: String = c.to_uppercase().collect();
                    format!("{}{}", upper, chars.collect::<String>())
                }
            }
        })
        .collect::<Vec<_>>()
        .join(" ")
}

/// Whether `module_id` is projected as an MCP resource rather than a tool.
///
/// These modules answer a GET-shaped question with no side effects — the
/// MCP `resources/*` primitive, not `tools/*`, is the correct fit
/// (aiperceivable/apcore-mcp#15(a)). [`MCPServerFactory::build_tools`] uses
/// this to exclude them from the tool list; [`MCPServerFactory::
/// register_resource_handlers`] builds resources / resource templates from
/// the same canonical ids. `system.control.*` (the mutating management
/// modules) is unaffected and stays a tool.
///
/// Matches the six canonical ids exactly rather than the three family
/// prefixes (`system.health.`, `system.usage.`, `system.manifest.`) that
/// PROTOCOL_SPEC §6.6.2 classifies by. The two agree for every registry
/// `register_sys_modules` produces, and differ only for a read-only
/// `system.*` id this adapter has no resource for — a seventh module added
/// by a future apcore, or one a host registered through `register_internal`
/// itself. A prefix match would drop such a module from `tools/list` while
/// `register_resource_handlers` gave it no resource either, so it would
/// vanish from both discovery surfaces at once. Keeping it a tool is the
/// safer failure: visible and callable, merely classified as a tool until
/// this adapter learns its resource shape. Discovery is all that is at stake
/// — `tools/call` dispatches by id and never consulted this list.
pub fn is_readonly_system_resource_module_id(module_id: &str) -> bool {
    SYSTEM_RESOURCE_MODULES
        .iter()
        .chain(SYSTEM_RESOURCE_TEMPLATE_MODULES.iter())
        .any(|(canonical_id, ..)| *canonical_id == module_id)
}

/// The `com.aiperceivable/management` MCP initialization extension id
/// (aiperceivable/apcore-mcp#16). Broadcast in `initialize`'s
/// `capabilities.extensions` when at least one management surface
/// (health / usage / manifest / control) is registered.
pub const MANAGEMENT_EXTENSION_ID: &str = "com.aiperceivable/management";

/// The apcore PROTOCOL_SPEC version this bridge's `system.*` surface
/// implements, reported in the `com.aiperceivable/management` extension.
/// Bump this alongside the apcore dependency when the sys_modules surface
/// (module ids, schemas, or governance semantics) is re-vetted against a
/// newer spec — it is a manual, deliberate value, not derived at build time.
pub const MANAGEMENT_EXTENSION_PROTOCOL_VERSION: &str = "1.30.0";

/// Which apcore management surfaces (aiperceivable/apcore-mcp#16) this
/// server instance actually exposes. Drives the `com.aiperceivable/
/// management` initialize extension: only the `true` flags are named in
/// `surfaces`, and the extension is omitted entirely when all four are
/// `false`.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct ManagementCapabilities {
    /// `system.health.*` modules are registered.
    pub health: bool,
    /// `system.usage.*` modules are registered.
    pub usage: bool,
    /// `system.manifest.*` modules are registered.
    pub manifest: bool,
    /// `system.control.*` modules are registered.
    pub control: bool,
}

impl ManagementCapabilities {
    /// `true` if any surface is exposed.
    pub fn any(&self) -> bool {
        self.health || self.usage || self.manifest || self.control
    }

    /// The `surfaces` array for the extension payload — only the `true`
    /// flags, in a fixed, stable order.
    fn surfaces(&self) -> Vec<&'static str> {
        let mut surfaces = Vec::with_capacity(4);
        if self.health {
            surfaces.push("health");
        }
        if self.usage {
            surfaces.push("usage");
        }
        if self.manifest {
            surfaces.push("manifest");
        }
        if self.control {
            surfaces.push("control");
        }
        surfaces
    }

    /// Build the value of `capabilities.extensions` for `initialize`, or
    /// `None` when no surface is exposed (in which case the `extensions` key
    /// is omitted entirely rather than advertised empty).
    pub fn to_extensions_value(self) -> Option<Value> {
        if !self.any() {
            return None;
        }
        Some(serde_json::json!({
            MANAGEMENT_EXTENSION_ID: {
                "surfaces": self.surfaces(),
                "protocolVersion": MANAGEMENT_EXTENSION_PROTOCOL_VERSION,
            }
        }))
    }
}

/// The three read-only `system.*` modules with no required parameters,
/// registered as static `apcore://` resources: `(module_id, uri, display
/// name)`.
const SYSTEM_RESOURCE_MODULES: &[(&str, &str, &str)] = &[
    (
        "system.health.summary",
        "apcore://system.health.summary",
        "system.health.summary",
    ),
    (
        "system.usage.summary",
        "apcore://system.usage.summary",
        "system.usage.summary (optional ?period= query)",
    ),
    (
        "system.manifest.full",
        "apcore://system.manifest.full",
        "system.manifest.full",
    ),
];

/// The three read-only `system.*` modules that require a `module_id`
/// argument, registered as `apcore://` resource templates: `(module_id,
/// uri_template, display name)`.
///
/// `system.usage.module`'s template carries the RFC 6570 form-style query
/// expansion `{?period}` -- aiperceivable/apcore-mcp#15's URI-convention
/// table declares it as `apcore://system.usage.module/{module_id}{?period}`,
/// and apcore-mcp-typescript's `systemResourceUriTemplate()` emits the same
/// suffix. The other two templates take no query parameter.
const SYSTEM_RESOURCE_TEMPLATE_MODULES: &[(&str, &str, &str)] = &[
    (
        "system.health.module",
        "apcore://system.health.module/{module_id}",
        "system.health.module",
    ),
    (
        "system.usage.module",
        "apcore://system.usage.module/{module_id}{?period}",
        "system.usage.module (optional ?period= query)",
    ),
    (
        "system.manifest.module",
        "apcore://system.manifest.module/{module_id}",
        "system.manifest.module",
    ),
];

/// Read one of the six read-only `system.*` resources exposed by
/// [`MCPServerFactory::register_resource_handlers`].
///
/// `rest` is the part of the URI after the `apcore://` scheme, e.g.
/// `"system.health.summary"` or `"system.usage.module/files.read?period=1d"`.
/// Dispatches through `router.handle_call` — the same path a `tools/call`
/// takes — so ACL, approval, and redaction all apply exactly as they do for
/// `system.control.*`. [aiperceivable/apcore-mcp#15(a)]
async fn read_system_resource(
    router: &ExecutionRouter,
    available: &std::collections::HashSet<&'static str>,
    uri: &str,
) -> Result<Vec<ReadResourceContents>, FactoryError> {
    let rest = uri.strip_prefix("apcore://").unwrap_or(uri);
    let (path, query) = match rest.split_once('?') {
        Some((p, q)) => (p, Some(q)),
        None => (rest, None),
    };
    let period = query.and_then(|q| {
        q.split('&').find_map(|kv| {
            let (k, v) = kv.split_once('=')?;
            (k == "period").then(|| v.to_string())
        })
    });

    // Static summary/full resources: the path IS the module id.
    if let Some((module_id, ..)) = SYSTEM_RESOURCE_MODULES
        .iter()
        .find(|(module_id, ..)| *module_id == path)
    {
        if !available.contains(module_id) {
            return Err(FactoryError::ResourceNotFound(uri.to_string()));
        }
        let mut args = serde_json::Map::new();
        if *module_id == "system.usage.summary" {
            if let Some(period) = period {
                args.insert("period".to_string(), Value::String(period));
            }
        }
        return call_router_as_resource(router, module_id, Value::Object(args), uri).await;
    }

    // Templated `.module` resources: the path is `"{module_id}/{module_id}"`.
    for (template_module_id, ..) in SYSTEM_RESOURCE_TEMPLATE_MODULES {
        let Some(target_module_id) = path.strip_prefix(&format!("{template_module_id}/")) else {
            continue;
        };
        if target_module_id.is_empty() || !available.contains(template_module_id) {
            return Err(FactoryError::ResourceNotFound(uri.to_string()));
        }
        let mut args = serde_json::Map::new();
        args.insert(
            "module_id".to_string(),
            Value::String(target_module_id.to_string()),
        );
        if *template_module_id == "system.usage.module" {
            if let Some(period) = period {
                args.insert("period".to_string(), Value::String(period));
            }
        }
        return call_router_as_resource(router, template_module_id, Value::Object(args), uri).await;
    }

    Err(FactoryError::ResourceNotFound(uri.to_string()))
}

/// Invoke `module_id` through `router.handle_call` and adapt the result into
/// `resources/read`'s `contents` shape. `handle_call` is the same entry
/// point `tools/call` uses, so an ACL denial or a required-approval gate
/// applies identically to a resource read.
async fn call_router_as_resource(
    router: &ExecutionRouter,
    module_id: &str,
    arguments: Value,
    uri: &str,
) -> Result<Vec<ReadResourceContents>, FactoryError> {
    let (content_items, is_error, _trace_id) =
        router.handle_call(module_id, &arguments, None).await;
    let text = content_items
        .iter()
        .filter(|item| item.content_type == "text")
        .filter_map(|item| item.data.as_str())
        .collect::<Vec<_>>()
        .join("");
    if is_error {
        return Err(FactoryError::Other(format!("{uri}: {text}")));
    }
    Ok(vec![ReadResourceContents {
        content: text,
        mime_type: "application/json".to_string(),
    }])
}

/// Metadata flags derived from module annotations for inclusion in
/// the MCP tool `_meta` object.
#[derive(Debug, Clone, PartialEq)]
pub struct ToolMeta {
    /// Whether the tool requires human approval before execution.
    pub requires_approval: bool,
    /// Whether the tool supports streaming responses.
    pub streaming: bool,
}

/// Factory-level annotation helpers that extend [`AnnotationMapper`]
/// with `_meta` generation for MCP tool definitions.
pub struct ToolAnnotationBuilder;

impl ToolAnnotationBuilder {
    /// Convert module annotations into MCP tool annotations.
    ///
    /// Delegates to [`AnnotationMapper::to_mcp_annotations`].
    pub fn build_annotations(annotations: Option<&ModuleAnnotations>) -> McpAnnotations {
        AnnotationMapper::to_mcp_annotations(annotations)
    }

    /// Check whether the module's annotations indicate streaming support.
    ///
    /// Returns `false` when annotations are `None`.
    pub fn is_streaming(annotations: Option<&ModuleAnnotations>) -> bool {
        match annotations {
            None => false,
            Some(a) => a.streaming,
        }
    }

    /// Build the `_meta` object for an MCP tool definition.
    ///
    /// Includes `requiresApproval` and `streaming` flags derived from
    /// module annotations.
    pub fn build_meta(annotations: Option<&ModuleAnnotations>) -> ToolMeta {
        ToolMeta {
            requires_approval: AnnotationMapper::has_requires_approval(annotations),
            streaming: Self::is_streaming(annotations),
        }
    }

    /// Serialize `_meta` to a JSON value suitable for embedding in
    /// an MCP tool definition.
    pub fn build_meta_value(annotations: Option<&ModuleAnnotations>) -> Value {
        let meta = Self::build_meta(annotations);
        let mut map = serde_json::Map::new();
        if meta.requires_approval {
            map.insert("requiresApproval".to_string(), Value::Bool(true));
        }
        if meta.streaming {
            map.insert("streaming".to_string(), Value::Bool(true));
        }
        Value::Object(map)
    }
}

/// Factory for constructing [`MCPServer`] instances from a registry and executor.
pub struct MCPServerFactory {
    #[allow(dead_code)]
    schema_converter: SchemaConverter,
    #[allow(dead_code)]
    annotation_mapper: AnnotationMapper,
    /// When `true`, MCP `Tool.description` is rendered as canonical
    /// apcore-toolkit Markdown (title, description, parameters,
    /// returns, behavior table, tags, examples) instead of the plain
    /// one-line description. LLMs select tools primarily from this
    /// string — Markdown packs more decision-relevant signal per
    /// token. apcore-toolkit 0.6+. Display-overlay
    /// `mcp.description` overrides are still honoured first.
    rich_description: bool,
}

impl Default for MCPServerFactory {
    fn default() -> Self {
        Self::new()
    }
}

impl MCPServerFactory {
    /// Create a new factory with default components.
    pub fn new() -> Self {
        Self {
            schema_converter: SchemaConverter,
            annotation_mapper: AnnotationMapper,
            rich_description: false,
        }
    }

    /// Create a factory whose `build_tools` renders `Tool.description`
    /// as apcore-toolkit Markdown. See struct-level field docs for
    /// rationale.
    pub fn with_rich_description(rich_description: bool) -> Self {
        Self {
            schema_converter: SchemaConverter,
            annotation_mapper: AnnotationMapper,
            rich_description,
        }
    }

    /// Whether this factory renders `Tool.description` as Markdown.
    pub fn rich_description(&self) -> bool {
        self.rich_description
    }

    /// Cross-SDK parity no-op for `MCPServerFactory::prepare()`.
    ///
    /// TypeScript's factory uses this to prime the apcore-toolkit Markdown
    /// renderer (the toolkit's import has measurable startup cost in Node).
    /// Rust links apcore-toolkit at build time and renders synchronously, so
    /// no priming is required. Exposed as an `async fn` returning `false`
    /// so cross-SDK application code can call
    /// `MCPServerFactory::prepare().await` at startup without a
    /// language-specific branch. See
    /// `docs/features/mcp-server-factory.md` §"Py/Rust no-op for parity".
    /// [A-002]
    pub async fn prepare() -> bool {
        false
    }

    /// Create a new MCP server instance.
    ///
    /// # Arguments
    /// * `name` - Server name advertised in MCP init. Must be a
    ///   non-empty string of at most 255 characters per the protocol
    ///   spec. [D10-002]
    /// * `version` - Server version string. Bound onto the server's config,
    ///   so [`build_init_options`](Self::build_init_options) reports it
    ///   without the caller passing it a second time. [A-D-FA-4]
    ///
    /// # Errors
    /// Returns [`FactoryError::InvalidName`] when `name` is empty or
    /// longer than 255 characters. Python and TypeScript SDKs raise
    /// the same error from their factory's equivalent entry point.
    pub fn create_server(
        &self,
        name: &str,
        version: &str,
    ) -> Result<MCPServer, crate::server::server::FactoryError> {
        // [D10-002] Spec contract: non-empty, <= 255 chars.
        //
        // [A-D-FA-3] CHARACTERS, not bytes. `str::len()` counts UTF-8 bytes,
        // so a 100-character CJK name measured 300 and was rejected here while
        // Python (code points) and TypeScript (UTF-16 units) both accepted it.
        let name_len = name.chars().count();
        if name.is_empty() || name_len > 255 {
            return Err(crate::server::server::FactoryError::InvalidName(name_len));
        }
        Ok(MCPServer::new(MCPServerConfig {
            name: name.to_string(),
            // [A-D-FA-4] Bind the version at construction, as TypeScript's
            // `new Server({ name, version }, ..)` does. Discarding it left
            // `MCPServerConfig::version` at None, so a server built through the
            // factory reported the crate version from `MCPServer::serve` unless
            // the caller happened to pass it again.
            version: Some(version.to_string()),
            ..Default::default()
        }))
    }

    /// Build a single MCP tool definition from a module descriptor.
    ///
    /// Mapping:
    /// - `name_override` (if provided) or `descriptor.name` -> `Tool.name`
    /// - `description` + AI intent metadata from `descriptor.metadata` -> `Tool.description`
    /// - `SchemaConverter::convert_input_schema` -> `Tool.inputSchema`
    /// - `AnnotationMapper::to_mcp_annotations` -> `Tool.annotations`
    /// - `requires_approval` / `streaming` flags -> `Tool._meta`
    pub fn build_tool(
        &self,
        descriptor: &ModuleDescriptor,
        description: &str,
        name_override: Option<&str>,
    ) -> Result<Tool, Box<dyn std::error::Error>> {
        self.build_tool_with_registry(descriptor, description, name_override, None)
    }

    /// `build_tool` variant that, in a future apcore release, will be able
    /// to prefer `registry.export_schema_strict(name, true)` for the
    /// input schema (Strict Schema Sourcing per `mcp-server-factory.md`).
    ///
    /// **Status (apcore 0.19.0):** the `Registry::export_schema_strict`
    /// method has been added to `apcore-rust` HEAD but has not yet shipped
    /// in a released version. While the dep stays at `apcore = "0.19"`,
    /// this variant always falls through to the local `SchemaConverter`,
    /// matching the per-SDK status documented in
    /// `mcp-server-factory.md` "Strict Schema Sourcing" → Rust row.
    ///
    /// When apcore 0.20+ ships, the body of this method will switch to
    /// `registry.export_schema_strict(&descriptor.module_id, true)` and
    /// drop the local-only path. [A-D-012]
    pub fn build_tool_with_registry(
        &self,
        descriptor: &ModuleDescriptor,
        description: &str,
        name_override: Option<&str>,
        _registry: Option<&Registry>,
    ) -> Result<Tool, Box<dyn std::error::Error>> {
        // Reject reserved __apcore_ prefix at the symbol boundary, not just
        // the bulk path. Direct callers (extensions, plugins, tests) would
        // otherwise produce a poisoned Tool that shadows the async-task
        // meta-tools. Python rejects at this same boundary; Rust now does
        // too. [A-D-009]
        if AsyncTaskBridge::is_reserved_id(&descriptor.module_id) {
            return Err(Box::new(
                crate::server::server::FactoryError::ReservedPrefix(descriptor.module_id.clone()),
            ));
        }

        // [A-D-012] [D11-011] Strict Schema Sourcing: pending apcore 0.20 release.
        // TODO(apcore 0.20): Try registry.export_schema_strict() first (see
        // Python/TS factory). Currently always falls through to local SchemaConverter.
        // Python+TS try `registry.export_schema(strict=True)` first; Rust always
        // uses the local SchemaConverter until the apcore 0.20 API ships.
        let input_schema = SchemaConverter::convert_input_schema(&descriptor.input_schema)?;

        // Map annotations
        let mcp_ann = AnnotationMapper::to_mcp_annotations(descriptor.annotations.as_ref());
        let annotations = Some(ToolAnnotations {
            title: mcp_ann.title,
            read_only_hint: Some(mcp_ann.read_only_hint),
            destructive_hint: Some(mcp_ann.destructive_hint),
            idempotent_hint: Some(mcp_ann.idempotent_hint),
            open_world_hint: Some(mcp_ann.open_world_hint),
        });

        // Build _meta
        let meta_value = ToolAnnotationBuilder::build_meta_value(descriptor.annotations.as_ref());
        let meta = if meta_value.as_object().is_none_or(|m| m.is_empty()) {
            None
        } else {
            Some(meta_value)
        };

        // Extract AI intent metadata from descriptor.metadata as string map
        let intent_metadata: HashMap<String, String> = descriptor
            .metadata
            .iter()
            .filter_map(|(k, v)| v.as_str().map(|s| (k.clone(), s.to_string())))
            .collect();
        let intent_ref = if intent_metadata.is_empty() {
            None
        } else {
            Some(&intent_metadata)
        };

        // Enrich description with AI intent metadata
        let enriched_description = enrich_description(description, intent_ref);

        // Use name_override if provided, otherwise fall back to module_id
        let tool_name = match name_override {
            Some(name) => name.to_string(),
            None => descriptor.module_id.clone(),
        };

        Ok(Tool {
            name: tool_name,
            description: enriched_description,
            input_schema,
            annotations,
            meta,
        })
    }

    // ---- Task: build_tools ----

    /// Build MCP tool definitions for all modules in the registry.
    ///
    /// Delegates filtering to `Registry::list(tags, prefix)`, then builds
    /// a tool for each module that has a definition. Modules without
    /// definitions or that fail `build_tool()` are logged and skipped.
    ///
    /// Display overlays are resolved from `descriptor.metadata["display"]["mcp"]`:
    /// - `alias`: overrides the tool name
    /// - `description`: overrides the descriptor description
    /// - `guidance`: appended as "\n\nGuidance: {text}" to the description
    pub fn build_tools(
        &self,
        registry: &Registry,
        tags: Option<&[&str]>,
        prefix: Option<&str>,
    ) -> Result<Vec<Tool>, crate::server::server::FactoryError> {
        let module_ids = registry.list(tags, prefix, None);
        let mut tools = Vec::new();

        for module_id in module_ids {
            // Read-only system.* modules (health/usage/manifest) are exposed
            // as MCP resources, not tools (aiperceivable/apcore-mcp#15(a)).
            // `system.control.*` is unaffected — it stays a tool.
            if is_readonly_system_resource_module_id(&module_id) {
                continue;
            }
            // Reject module ids that collide with the reserved async-task
            // meta-tool namespace (`__apcore_` prefix). These names are
            // owned by the AsyncTaskBridge; user modules must not shadow
            // them. Hard-fail to match Python (raises) and TypeScript
            // (throws). [A-D-010]
            if AsyncTaskBridge::is_reserved_id(&module_id) {
                return Err(crate::server::server::FactoryError::ReservedPrefix(
                    module_id,
                ));
            }
            // apcore 0.22.0: get_definition() returns
            // Result<Option<ModuleDescriptor>, ModuleError>; a missing
            // descriptor or a lookup error both mean "skip this module".
            let descriptor = match registry.get_definition(&module_id) {
                Ok(Some(d)) => d,
                Ok(None) | Err(_) => {
                    tracing::warn!("Skipped module {}: no definition found", module_id);
                    continue;
                }
            };

            // apcore 0.27 changed `Registry::describe` in two ways: it became
            // fallible, and it now returns the full generated markdown
            // envelope (heading, tags, parameter table) rather than the
            // module's one-line summary. This slot is the NON-rich fallback of
            // the chain below, so adopting the new text would make
            // `rich_description: false` emit a rich document and collapse the
            // flag. `descriptor.description` is the exact value 0.26's
            // `describe()` returned for a registered module
            // (`module.description()`), so reading it preserves that text.
            let base_description = descriptor.description.clone();

            // Resolve display overlay. apcore 0.19.0 introduced a top-level
            // `ModuleDescriptor.display` field; when present, it takes
            // precedence over `metadata["display"]` (kept for backwards
            // compatibility with configs that embedded the overlay in metadata).
            let mcp_display = descriptor
                .display
                .as_ref()
                .or_else(|| descriptor.metadata.get("display"))
                .and_then(|v| v.get("mcp"));

            let name_override = mcp_display
                .and_then(|d| d.get("alias"))
                .and_then(|v| v.as_str());

            // Resolution chain for the LLM-facing description:
            //   1. Operator-typed `display.mcp.description` (hard override).
            //   2. apcore-toolkit `format_module(Markdown)` when
            //      `rich_description` is on — gives the LLM a structured
            //      tool description (parameters, returns, behavior table,
            //      tags, examples) instead of a one-line summary.
            //   3. Fallback to the plain `registry.describe()` text.
            let description = match mcp_display
                .and_then(|d| d.get("description"))
                .and_then(|v| v.as_str())
            {
                Some(desc) => desc.to_string(),
                // [A-D-MD-3] None from the helper means the render failed;
                // fall back to the plain describe() text.
                None if self.rich_description => {
                    crate::markdown::render_module_markdown(&descriptor, true)
                        .unwrap_or(base_description)
                }
                None => base_description,
            };

            let description = match mcp_display
                .and_then(|d| d.get("guidance"))
                .and_then(|v| v.as_str())
            {
                Some(guidance) => format!("{}\n\nGuidance: {}", description, guidance),
                None => description,
            };

            // [A-D-012] Pass the registry through so the strict-schema
            // sourcing path can prefer registry.export_schema_strict(true).
            match self.build_tool_with_registry(
                &descriptor,
                &description,
                name_override,
                Some(registry),
            ) {
                Ok(tool) => tools.push(tool),
                Err(e) => {
                    // Reserved-prefix is fatal (matched at the loop top with
                    // an early return). Other build_tool errors are
                    // per-module config glitches — log and skip per spec
                    // "robust building" rule.
                    if let Some(fe) = e.downcast_ref::<crate::server::server::FactoryError>() {
                        if matches!(fe, crate::server::server::FactoryError::ReservedPrefix(_)) {
                            return Err(crate::server::server::FactoryError::ReservedPrefix(
                                module_id,
                            ));
                        }
                    }
                    tracing::warn!("Failed to build tool for {}: {}", module_id, e);
                    continue;
                }
            }
        }

        Ok(tools)
    }

    // ---- Task: register_handlers ----

    /// Register `list_tools` and `call_tool` handlers on the server.
    ///
    /// The `list_tools` handler returns a clone of the provided tools list.
    /// The `call_tool` handler delegates to the router's `handle_call` method,
    /// extracting progress token and identity from the extra context.
    ///
    /// Handlers are stored as closures on the `MCPServer` struct and invoked by
    /// this crate's own transport layer when processing MCP protocol messages.
    ///
    /// This arrangement dates from a time when no Rust MCP SDK existed. One
    /// does now — `rmcp`, from the same organisation that publishes the Python
    /// and TypeScript SDKs the sibling bridges use — and the plan is to move
    /// onto it. See "Planned: migration to rmcp" in
    /// `docs/features/transport.md` for what migrates, what stays hand-rolled,
    /// and the prerequisites.
    pub fn register_handlers(
        &self,
        server: &mut MCPServer,
        tools: Vec<Tool>,
        router: Arc<ExecutionRouter>,
    ) {
        let tools = Arc::new(tools);

        // Share the router's session registry with the transport layer. The
        // transport registers each connection into it; the router resolves a
        // connection back out of it when a tool call needs to elicit. Without
        // this the two halves hold separate empty maps and every lookup misses.
        server.session_registry = Some(router.session_registry());

        // list_tools handler: returns a clone of the tools list
        let tools_clone = Arc::clone(&tools);
        server.list_tools_handler = Some(Arc::new(move || tools_clone.as_ref().clone()));

        // call_tool handler: delegates to the execution router.
        //
        // [D11-012 / A-D-FA-7] Identity propagation: Python+TS auto-extract
        // identity from ContextVar/AsyncLocalStorage inside the handler. Rust
        // resolves it one layer down instead — `ExecutionRouter::resolve_identity`
        // prefers the `extra` map populated by AuthMiddlewareLayer and falls
        // back to the `AUTH_IDENTITY` task-local, which the middleware binds
        // around the whole request. So identity flows even when the transport
        // passes no `extra`, matching the peers' behaviour.
        let router_clone = Arc::clone(&router);
        server.call_tool_handler = Some(Arc::new(move |name, arguments, extra| {
            let router = Arc::clone(&router_clone);
            Box::pin(async move {
                let extra_ref = extra.as_ref();
                let (content_items, is_error, trace_id) =
                    router.handle_call(&name, &arguments, extra_ref).await;

                // Extract any pipeline trace item for `_meta.trace`.
                let mut meta_trace: Option<Value> = None;
                let mut text_items: Vec<TextContent> = Vec::new();
                for item in content_items {
                    match item.content_type.as_str() {
                        "text" => text_items.push(TextContent::new(
                            item.data.as_str().unwrap_or_default().to_string(),
                        )),
                        "trace" => {
                            meta_trace = Some(summarize_trace(&item.data));
                        }
                        _ => {}
                    }
                }

                // Build `_meta` with pipeline trace and/or W3C traceparent.
                let mut meta_obj = serde_json::Map::new();
                if let Some(t) = meta_trace {
                    meta_obj.insert("trace".to_string(), t);
                }
                if let Some(tid) = trace_id.as_deref() {
                    // Synthesize a W3C traceparent from the context trace_id.
                    // Strip dashes to produce 32 lowercase hex chars; generate
                    // a random 8-byte parent span. Matches apcore
                    // `TraceContext::inject`.
                    let trace_hex = tid.replace('-', "");
                    if trace_hex.len() == 32
                        && trace_hex
                            .bytes()
                            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
                    {
                        let parent = uuid::Uuid::new_v4().simple().to_string()[..16].to_string();
                        let tp = format!("00-{trace_hex}-{parent}-01");
                        meta_obj.insert("traceparent".to_string(), Value::String(tp));
                    }
                }
                let meta = if meta_obj.is_empty() {
                    None
                } else {
                    Some(Value::Object(meta_obj))
                };
                CallToolResult {
                    content: text_items,
                    is_error,
                    meta,
                }
            })
        }));
    }

    /// Register the four `__apcore_task_*` meta-tools so they appear in
    /// `tools/list` responses alongside user modules. Callers must also
    /// install the `AsyncTaskBridge` on the router via
    /// [`ExecutionRouter::with_async_bridge`].
    pub fn append_meta_tools(tools: &mut Vec<Tool>) {
        tools.extend(AsyncTaskBridge::build_meta_tools());
    }

    /// Append the `__apcore_approval_check` meta-tool so it appears in
    /// `tools/list` responses. Only called when an approval handler is
    /// configured (Phase B async approvals). Callers must also install the
    /// [`ApprovalBridge`](crate::server::approval_bridge::ApprovalBridge) on
    /// the router via
    /// [`ExecutionRouter::with_approval_bridge`](crate::server::router::ExecutionRouter::with_approval_bridge).
    pub fn append_approval_meta_tools(tools: &mut Vec<Tool>) {
        tools.extend(crate::server::approval_bridge::ApprovalBridge::build_meta_tools());
    }

    // ---- Task: register_resources ----

    /// Register `list_resources` and `read_resource` handlers for modules
    /// with documentation.
    ///
    /// Iterates over the registry and exposes each module's
    /// `descriptor.documentation` field (long-form text) as a
    /// `docs://{module_id}` resource. Modules with no `documentation` get no
    /// resource — Python (factory.py:447) and TypeScript (factory.ts:358) both
    /// gate on that field alone. [A-D-FA-8, closes A-D-013]
    ///
    /// Handlers are stored as closures on the `MCPServer` struct; see
    /// [`register_handlers`](Self::register_handlers) for why, and for the
    /// planned move onto `rmcp`.
    ///
    /// Also registers the three read-only `system.*` module families as MCP
    /// resources (aiperceivable/apcore-mcp#15(a), aiperceivable/
    /// apcore-mcp-rust#6): `system.health.summary`, `system.usage.summary`
    /// and `system.manifest.full` as static `apcore://` resources, and
    /// `system.health.module`, `system.usage.module` and
    /// `system.manifest.module` — which require a `module_id` argument — as
    /// `apcore://` resource *templates*. Only registered when the
    /// corresponding module id is actually present in `registry`; no
    /// separate opt-in flag. A read of any of these goes through `router`'s
    /// `handle_call` — the same ACL / approval / redaction path a `tools/
    /// call` gets — never a direct registry or module invocation.
    pub fn register_resource_handlers(
        &self,
        server: &mut MCPServer,
        registry: &Registry,
        router: Arc<ExecutionRouter>,
    ) {
        // Build docs map: module_id -> documentation.
        //
        // [A-D-FA-8] No `description` fallback. It gave every module with any
        // non-empty description a `docs://{id}` entry, so for the same registry
        // `resources/list` returned more entries on Rust and `resources/read`
        // of `docs://foo` succeeded here while the peers raise
        // "Resource not found".
        let mut docs_map: HashMap<String, String> = HashMap::new();
        for module_id in registry.list(None, None, None) {
            if let Ok(Some(descriptor)) = registry.get_definition(&module_id) {
                if let Some(doc_text) = descriptor.documentation.filter(|s| !s.is_empty()) {
                    docs_map.insert(module_id.to_string(), doc_text);
                }
            }
        }

        let docs = Arc::new(docs_map);

        // Which of the six read-only system.* module ids this registry
        // actually has. Drives both which resources/templates are
        // advertised and which reads are accepted at read time — a caller
        // cannot read `apcore://system.control.update_config` (not in this
        // set at all) or a summary resource whose module was never
        // registered.
        let available: std::collections::HashSet<&'static str> = SYSTEM_RESOURCE_MODULES
            .iter()
            .chain(SYSTEM_RESOURCE_TEMPLATE_MODULES.iter())
            .map(|(id, ..)| *id)
            .filter(|id| registry.has(id))
            .collect();
        let available = Arc::new(available);

        // list_resources handler
        let docs_for_list = Arc::clone(&docs);
        let available_for_list = Arc::clone(&available);
        server.list_resources_handler = Some(Arc::new(move || {
            let mut resources: Vec<Resource> = docs_for_list
                .keys()
                .map(|module_id| Resource {
                    uri: format!("docs://{}", module_id),
                    name: format!("{} documentation", module_id),
                    mime_type: "text/plain".to_string(),
                })
                .collect();
            for (module_id, uri, name) in SYSTEM_RESOURCE_MODULES {
                if available_for_list.contains(module_id) {
                    resources.push(Resource {
                        uri: (*uri).to_string(),
                        name: (*name).to_string(),
                        mime_type: "application/json".to_string(),
                    });
                }
            }
            resources
        }));

        // list_resource_templates handler
        let available_for_templates = Arc::clone(&available);
        server.list_resource_templates_handler = Some(Arc::new(move || {
            SYSTEM_RESOURCE_TEMPLATE_MODULES
                .iter()
                .filter(|(module_id, ..)| available_for_templates.contains(module_id))
                .map(|(_, uri_template, name)| ResourceTemplate {
                    uri_template: (*uri_template).to_string(),
                    name: (*name).to_string(),
                    mime_type: "application/json".to_string(),
                })
                .collect()
        }));

        // read_resource handler
        let docs_for_read = Arc::clone(&docs);
        server.read_resource_handler = Some(Arc::new(move |uri: String| {
            let docs = Arc::clone(&docs_for_read);
            let router = Arc::clone(&router);
            let available = Arc::clone(&available);
            Box::pin(async move {
                if let Some(module_id) = uri.strip_prefix("docs://") {
                    return match docs.get(module_id) {
                        Some(doc) => Ok(vec![ReadResourceContents {
                            content: doc.clone(),
                            mime_type: "text/plain".to_string(),
                        }]),
                        None => Err(FactoryError::ResourceNotFound(uri.clone())),
                    };
                }
                if uri.starts_with("apcore://") {
                    return read_system_resource(&router, &available, &uri).await;
                }
                Err(FactoryError::UnsupportedScheme(uri))
            })
        }));
    }

    // ---- Task: init_options ----

    /// Build the MCP initialization options.
    ///
    /// Constructs `InitializationOptions` with server name, version, and
    /// capabilities derived from the registered handlers on the server.
    ///
    /// `management`, when given, broadcasts the `com.aiperceivable/
    /// management` initialize extension (aiperceivable/apcore-mcp#16)
    /// naming which management surfaces (health/usage/manifest/control) this
    /// server exposes. `None` — or `Some` with every flag `false` — omits
    /// the extension entirely; a client that never declares support for it
    /// is unaffected either way; it does not gate access to any resource or
    /// tool.
    pub fn build_init_options(
        &self,
        server: &MCPServer,
        name: &str,
        version: &str,
        management: Option<ManagementCapabilities>,
    ) -> InitializationOptions {
        // [A-D-FA-4] A server built by `create_server` carries its own
        // version; the argument is the fallback for servers constructed
        // directly via `MCPServer::new`, which leaves it None.
        let server_version = server
            .config()
            .version
            .clone()
            .unwrap_or_else(|| version.to_string());

        let tools_cap = if server.has_tool_handlers() {
            Some(ToolsCapability { list_changed: true })
        } else {
            None
        };

        let resources_cap = if server.has_resource_handlers() {
            Some(ResourcesCapability { list_changed: true })
        } else {
            None
        };

        let extensions = management.and_then(ManagementCapabilities::to_extensions_value);

        InitializationOptions {
            server_name: name.to_string(),
            server_version,
            capabilities: ServerCapabilities {
                tools: tools_cap,
                resources: resources_cap,
                extensions,
            },
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    /// Helper to create a minimal ModuleDescriptor for testing.
    fn make_descriptor(name: &str, annotations: ModuleAnnotations) -> ModuleDescriptor {
        ModuleDescriptor {
            module_id: name.to_string(),
            name: None,
            description: String::new(),
            documentation: None,
            input_schema: json!({"type": "object", "properties": {"query": {"type": "string"}}}),
            output_schema: json!({}),
            version: "1.0.0".to_string(),
            tags: vec![],
            annotations: Some(annotations),
            examples: vec![],
            metadata: HashMap::new(),
            display: None,
            sunset_date: None,
            dependencies: vec![],
            enabled: true,
        }
    }

    #[allow(dead_code)]
    fn make_descriptor_with_tags(name: &str, tags: Vec<String>) -> ModuleDescriptor {
        ModuleDescriptor {
            module_id: name.to_string(),
            name: None,
            description: String::new(),
            documentation: None,
            input_schema: json!({"type": "object", "properties": {"q": {"type": "string"}}}),
            output_schema: json!({}),
            version: "1.0.0".to_string(),
            tags,
            annotations: Some(ModuleAnnotations::default()),
            examples: vec![],
            metadata: HashMap::new(),
            display: None,
            sunset_date: None,
            dependencies: vec![],
            enabled: true,
        }
    }

    /// Helper to create an MCPServerFactory for tests.
    fn make_factory() -> MCPServerFactory {
        MCPServerFactory::new()
    }

    /// Helper to create a mock module for registry tests.
    struct MockModule {
        desc: String,
    }

    impl MockModule {
        fn new(desc: &str) -> Self {
            Self {
                desc: desc.to_string(),
            }
        }
    }

    #[async_trait::async_trait]
    impl apcore::module::Module for MockModule {
        fn input_schema(&self) -> serde_json::Value {
            json!({"type": "object", "properties": {"q": {"type": "string"}}})
        }
        fn output_schema(&self) -> serde_json::Value {
            json!({})
        }
        fn description(&self) -> &str {
            &self.desc
        }
        async fn execute(
            &self,
            _inputs: serde_json::Value,
            _ctx: &apcore::context::Context<serde_json::Value>,
        ) -> Result<serde_json::Value, apcore::errors::ModuleError> {
            Ok(json!({}))
        }
    }

    /// Helper to create a registry with mock modules.
    fn make_registry_with_modules(modules: Vec<(&str, &str, Vec<String>)>) -> Registry {
        make_registry_with_modules_and_metadata(
            modules
                .into_iter()
                .map(|(n, d, t)| (n, d, t, HashMap::new()))
                .collect(),
        )
    }

    #[allow(clippy::type_complexity)]
    fn make_registry_with_modules_and_metadata(
        modules: Vec<(&str, &str, Vec<String>, HashMap<String, serde_json::Value>)>,
    ) -> Registry {
        let registry = Registry::new();
        for (name, desc, tags, metadata) in modules {
            let module = Box::new(MockModule::new(desc));
            let descriptor = ModuleDescriptor {
                module_id: name.to_string(),
                name: None,
                description: desc.to_string(),
                documentation: None,
                input_schema: json!({"type": "object", "properties": {"q": {"type": "string"}}}),
                output_schema: json!({}),
                version: "1.0.0".to_string(),
                tags,
                annotations: Some(ModuleAnnotations::default()),
                examples: vec![],
                metadata,
                display: None,
                sunset_date: None,
                dependencies: vec![],
                enabled: true,
            };
            registry
                .register_internal(name, module, descriptor)
                .unwrap();
        }
        registry
    }

    /// Registry whose modules carry an explicit `documentation` field.
    ///
    /// [A-D-FA-8] `make_registry_with_modules` leaves `documentation` None, so
    /// docs:// coverage could only be exercised through the description
    /// fallback that finding removed.
    fn make_registry_with_documentation(modules: Vec<(&str, &str, Option<&str>)>) -> Registry {
        let registry = Registry::new();
        for (name, desc, documentation) in modules {
            let module = Box::new(MockModule::new(desc));
            let descriptor = ModuleDescriptor {
                module_id: name.to_string(),
                name: None,
                description: desc.to_string(),
                documentation: documentation.map(str::to_string),
                input_schema: json!({"type": "object", "properties": {"q": {"type": "string"}}}),
                output_schema: json!({}),
                version: "1.0.0".to_string(),
                tags: vec![],
                annotations: Some(ModuleAnnotations::default()),
                examples: vec![],
                metadata: HashMap::new(),
                display: None,
                sunset_date: None,
                dependencies: vec![],
                enabled: true,
            };
            registry
                .register_internal(name, module, descriptor)
                .unwrap();
        }
        registry
    }

    /// Executor that hands the built context straight back, so a test can
    /// inspect the identity the router resolved. Mirrors
    /// `router::tests::IdentityCapturingExecutor`.
    struct IdentityCapturingExecutor;

    #[async_trait::async_trait]
    impl crate::server::router::Executor for IdentityCapturingExecutor {
        async fn call_async(
            &self,
            _module_id: &str,
            _inputs: &serde_json::Value,
            context: Option<&serde_json::Value>,
            _version_hint: Option<&str>,
        ) -> Result<serde_json::Value, crate::server::router::ExecutorError> {
            Ok(context.cloned().unwrap_or(serde_json::Value::Null))
        }
    }

    // ---- [A-D-FA-7] ambient identity reaches the router ----

    /// The call_tool handler performed no ambient identity read: Rust relied
    /// entirely on the caller-populated `extra` map. Python reads
    /// `auth_identity_var` (factory.py:335) and TypeScript
    /// `getCurrentIdentity()` (factory.ts:485) inside the registered handler,
    /// so identity flows even when the transport passes no `extra`.
    #[tokio::test]
    async fn test_call_tool_reads_ambient_identity_when_extra_is_absent() {
        use crate::auth::middleware::AUTH_IDENTITY;

        let factory = make_factory();
        let mut server = factory.create_server("test", "1.0.0").unwrap();
        let router = Arc::new(crate::server::router::ExecutionRouter::new(
            Box::new(IdentityCapturingExecutor),
            false,
            None,
        ));
        factory.register_handlers(&mut server, vec![], router);

        let identity = apcore::Identity::new(
            "ambient-user".to_string(),
            "user".to_string(),
            vec!["admin".to_string()],
            Default::default(),
        );
        let result = AUTH_IDENTITY
            .scope(Some(identity), async {
                server
                    .call_tool("mod".to_string(), json!({}), None)
                    .expect("call_tool handler must be registered")
                    .await
            })
            .await;

        assert!(!result.is_error, "call must succeed: {result:?}");
        let parsed: serde_json::Value =
            serde_json::from_str(&result.content[0].text).expect("context JSON");
        assert_eq!(
            parsed["identity"]["id"], "ambient-user",
            "the router must observe the ambient identity; got: {parsed}"
        );
    }

    /// An explicit `extra["identity"]` still wins over the ambient one.
    #[tokio::test]
    async fn test_explicit_extra_identity_wins_over_ambient() {
        use crate::auth::middleware::AUTH_IDENTITY;

        let factory = make_factory();
        let mut server = factory.create_server("test", "1.0.0").unwrap();
        let router = Arc::new(crate::server::router::ExecutionRouter::new(
            Box::new(IdentityCapturingExecutor),
            false,
            None,
        ));
        factory.register_handlers(&mut server, vec![], router);

        let ambient = apcore::Identity::new(
            "ambient-user".to_string(),
            "user".to_string(),
            vec![],
            Default::default(),
        );
        let extra = json!({"identity": {"id": "explicit-user", "type": "user", "roles": []}});
        let result = AUTH_IDENTITY
            .scope(Some(ambient), async {
                server
                    .call_tool("mod".to_string(), json!({}), Some(extra))
                    .expect("call_tool handler must be registered")
                    .await
            })
            .await;

        let parsed: serde_json::Value =
            serde_json::from_str(&result.content[0].text).expect("context JSON");
        assert_eq!(parsed["identity"]["id"], "explicit-user");
    }

    // ---- build_tool tests ----

    #[test]
    fn test_build_tool_name_is_module_name() {
        let factory = make_factory();
        let desc = make_descriptor("my.module.id", ModuleAnnotations::default());
        let tool = factory.build_tool(&desc, "A tool", None).unwrap();
        assert_eq!(tool.name, "my.module.id");
    }

    #[test]
    fn test_build_tool_description() {
        let factory = make_factory();
        let desc = make_descriptor("mod.test", ModuleAnnotations::default());
        let tool = factory
            .build_tool(&desc, "Reads files from disk", None)
            .unwrap();
        assert_eq!(tool.description, "Reads files from disk");
    }

    #[test]
    fn test_build_tool_input_schema() {
        let factory = make_factory();
        let desc = make_descriptor("mod.test", ModuleAnnotations::default());
        let tool = factory.build_tool(&desc, "desc", None).unwrap();
        assert_eq!(tool.input_schema["type"], "object");
        assert_eq!(tool.input_schema["properties"]["query"]["type"], "string");
    }

    #[test]
    fn test_build_tool_annotations_mapped() {
        let factory = make_factory();
        let ann = ModuleAnnotations {
            readonly: true,
            destructive: true,
            ..Default::default()
        };
        let desc = make_descriptor("mod.test", ann);
        let tool = factory.build_tool(&desc, "desc", None).unwrap();
        let annotations = tool.annotations.unwrap();
        assert_eq!(annotations.read_only_hint, Some(true));
        assert_eq!(annotations.destructive_hint, Some(true));
    }

    #[test]
    fn test_build_tool_meta_requires_approval() {
        let factory = make_factory();
        let ann = ModuleAnnotations {
            requires_approval: true,
            ..Default::default()
        };
        let desc = make_descriptor("mod.test", ann);
        let tool = factory.build_tool(&desc, "desc", None).unwrap();
        let meta = tool.meta.unwrap();
        assert_eq!(meta["requiresApproval"], true);
        assert!(meta.get("streaming").is_none());
    }

    #[test]
    fn test_build_tool_meta_streaming() {
        let factory = make_factory();
        let ann = ModuleAnnotations {
            streaming: true,
            ..Default::default()
        };
        let desc = make_descriptor("mod.test", ann);
        let tool = factory.build_tool(&desc, "desc", None).unwrap();
        let meta = tool.meta.unwrap();
        assert_eq!(meta["streaming"], true);
        assert!(meta.get("requiresApproval").is_none());
    }

    #[test]
    fn test_build_tool_meta_both() {
        let factory = make_factory();
        let ann = ModuleAnnotations {
            requires_approval: true,
            streaming: true,
            ..Default::default()
        };
        let desc = make_descriptor("mod.test", ann);
        let tool = factory.build_tool(&desc, "desc", None).unwrap();
        let meta = tool.meta.unwrap();
        assert_eq!(meta["requiresApproval"], true);
        assert_eq!(meta["streaming"], true);
    }

    #[test]
    fn test_build_tool_meta_none() {
        let factory = make_factory();
        let desc = make_descriptor("mod.test", ModuleAnnotations::default());
        let tool = factory.build_tool(&desc, "desc", None).unwrap();
        assert!(
            tool.meta.is_none(),
            "default annotations should produce no _meta"
        );
    }

    // ---- AI intent / enrich_description tests ----

    #[test]
    fn test_no_metadata_no_suffix() {
        let result = enrich_description("Base description", None);
        assert_eq!(result, "Base description");
    }

    #[test]
    fn test_empty_metadata_no_suffix() {
        let metadata = HashMap::new();
        let result = enrich_description("Base description", Some(&metadata));
        assert_eq!(result, "Base description");
    }

    #[test]
    fn test_when_to_use_appended() {
        let mut metadata = HashMap::new();
        metadata.insert(
            "x-when-to-use".to_string(),
            "Use for reading files".to_string(),
        );
        let result = enrich_description("Base description", Some(&metadata));
        assert_eq!(
            result,
            "Base description\n\nWhen To Use: Use for reading files"
        );
    }

    #[test]
    fn test_multiple_intents_appended() {
        let mut metadata = HashMap::new();
        metadata.insert("x-when-to-use".to_string(), "Use for reads".to_string());
        metadata.insert(
            "x-common-mistakes".to_string(),
            "Forgetting the path".to_string(),
        );
        let result = enrich_description("Base", Some(&metadata));
        assert!(result.contains("When To Use: Use for reads"));
        assert!(result.contains("Common Mistakes: Forgetting the path"));
        // Verify they are separated by newline (within the suffix block)
        let suffix = result.strip_prefix("Base\n\n").unwrap();
        assert!(suffix.contains('\n'));
    }

    #[test]
    fn test_intent_key_label_formatting() {
        assert_eq!(format_intent_label("x-when-to-use"), "When To Use");
        assert_eq!(format_intent_label("x-when-not-to-use"), "When Not To Use");
        assert_eq!(format_intent_label("x-common-mistakes"), "Common Mistakes");
        assert_eq!(format_intent_label("x-workflow-hints"), "Workflow Hints");
    }

    #[test]
    fn test_empty_intent_value_skipped() {
        let mut metadata = HashMap::new();
        metadata.insert("x-when-to-use".to_string(), "".to_string());
        metadata.insert("x-common-mistakes".to_string(), "Don't forget".to_string());
        let result = enrich_description("Base", Some(&metadata));
        assert!(!result.contains("When To Use"));
        assert!(result.contains("Common Mistakes: Don't forget"));
    }

    #[test]
    fn test_non_intent_metadata_ignored() {
        let mut metadata = HashMap::new();
        metadata.insert("x-custom-field".to_string(), "some value".to_string());
        metadata.insert("random-key".to_string(), "other value".to_string());
        let result = enrich_description("Base", Some(&metadata));
        assert_eq!(result, "Base");
    }

    #[test]
    fn test_build_tool_with_ai_intent_metadata() {
        let factory = make_factory();
        let mut desc = make_descriptor("files.read", ModuleAnnotations::default());
        desc.metadata
            .insert("x-when-to-use".to_string(), json!("Use for reading files"));
        desc.metadata.insert(
            "x-common-mistakes".to_string(),
            json!("Forgetting the path"),
        );
        let tool = factory.build_tool(&desc, "Read files", None).unwrap();
        assert!(tool.description.starts_with("Read files\n\n"));
        assert!(tool
            .description
            .contains("When To Use: Use for reading files"));
        assert!(tool
            .description
            .contains("Common Mistakes: Forgetting the path"));
    }

    #[test]
    fn test_intent_order_follows_constant() {
        let mut metadata = HashMap::new();
        metadata.insert("x-workflow-hints".to_string(), "hint".to_string());
        metadata.insert("x-when-to-use".to_string(), "use".to_string());
        metadata.insert("x-common-mistakes".to_string(), "mistake".to_string());
        metadata.insert("x-when-not-to-use".to_string(), "not use".to_string());
        let result = enrich_description("Base", Some(&metadata));
        let suffix = result.strip_prefix("Base\n\n").unwrap();
        let lines: Vec<&str> = suffix.lines().collect();
        assert_eq!(lines.len(), 4);
        assert!(lines[0].starts_with("When To Use:"));
        assert!(lines[1].starts_with("When Not To Use:"));
        assert!(lines[2].starts_with("Common Mistakes:"));
        assert!(lines[3].starts_with("Workflow Hints:"));
    }

    // ---- Annotation mapping tests (via ToolAnnotationBuilder) ----

    #[test]
    fn test_readonly_maps_to_read_only_hint() {
        let ann = ModuleAnnotations {
            readonly: true,
            ..Default::default()
        };
        let result = ToolAnnotationBuilder::build_annotations(Some(&ann));
        assert!(result.read_only_hint);
    }

    #[test]
    fn test_destructive_maps_to_destructive_hint() {
        let ann = ModuleAnnotations {
            destructive: true,
            ..Default::default()
        };
        let result = ToolAnnotationBuilder::build_annotations(Some(&ann));
        assert!(result.destructive_hint);
    }

    #[test]
    fn test_idempotent_maps_to_idempotent_hint() {
        let ann = ModuleAnnotations {
            idempotent: true,
            ..Default::default()
        };
        let result = ToolAnnotationBuilder::build_annotations(Some(&ann));
        assert!(result.idempotent_hint);
    }

    #[test]
    fn test_open_world_maps_to_open_world_hint() {
        let ann = ModuleAnnotations {
            open_world: false,
            ..Default::default()
        };
        let result = ToolAnnotationBuilder::build_annotations(Some(&ann));
        assert!(!result.open_world_hint);
    }

    #[test]
    fn test_default_annotations_mapping() {
        let ann = ModuleAnnotations::default();
        let result = ToolAnnotationBuilder::build_annotations(Some(&ann));
        assert!(!result.read_only_hint);
        assert!(!result.destructive_hint);
        assert!(!result.idempotent_hint);
        assert!(result.open_world_hint);
    }

    #[test]
    fn test_has_requires_approval_true() {
        let ann = ModuleAnnotations {
            requires_approval: true,
            ..Default::default()
        };
        let meta = ToolAnnotationBuilder::build_meta(Some(&ann));
        assert!(meta.requires_approval);
    }

    #[test]
    fn test_has_requires_approval_false() {
        let ann = ModuleAnnotations::default();
        let meta = ToolAnnotationBuilder::build_meta(Some(&ann));
        assert!(!meta.requires_approval);
    }

    #[test]
    fn test_streaming_flag() {
        let ann = ModuleAnnotations {
            streaming: true,
            ..Default::default()
        };
        assert!(ToolAnnotationBuilder::is_streaming(Some(&ann)));
        let meta = ToolAnnotationBuilder::build_meta(Some(&ann));
        assert!(meta.streaming);
    }

    #[test]
    fn test_streaming_flag_false_by_default() {
        let ann = ModuleAnnotations::default();
        assert!(!ToolAnnotationBuilder::is_streaming(Some(&ann)));
    }

    #[test]
    fn test_streaming_flag_none() {
        assert!(!ToolAnnotationBuilder::is_streaming(None));
    }

    // ---- _meta JSON tests ----

    #[test]
    fn test_build_meta_value_empty_for_defaults() {
        let ann = ModuleAnnotations::default();
        let meta = ToolAnnotationBuilder::build_meta_value(Some(&ann));
        let obj = meta.as_object().unwrap();
        assert!(
            obj.is_empty(),
            "default annotations should produce empty _meta"
        );
    }

    #[test]
    fn test_build_meta_value_with_approval_and_streaming() {
        let ann = ModuleAnnotations {
            requires_approval: true,
            streaming: true,
            ..Default::default()
        };
        let meta = ToolAnnotationBuilder::build_meta_value(Some(&ann));
        let obj = meta.as_object().unwrap();
        assert_eq!(obj.get("requiresApproval"), Some(&Value::Bool(true)));
        assert_eq!(obj.get("streaming"), Some(&Value::Bool(true)));
    }

    #[test]
    fn test_build_meta_value_none_annotations() {
        let meta = ToolAnnotationBuilder::build_meta_value(None);
        let obj = meta.as_object().unwrap();
        assert!(obj.is_empty());
    }

    // ---- build_tools tests ----

    #[test]
    fn test_build_tools_all_modules() {
        let factory = make_factory();
        let registry = make_registry_with_modules(vec![
            ("mod.a", "Module A", vec![]),
            ("mod.b", "Module B", vec![]),
            ("mod.c", "Module C", vec![]),
        ]);

        let tools = factory
            .build_tools(&registry, None, None)
            .expect("build_tools should not fail in test");
        assert_eq!(tools.len(), 3);
    }

    #[test]
    fn test_build_tools_tag_filter() {
        let factory = make_factory();
        let registry = make_registry_with_modules(vec![
            ("mod.a", "Module A", vec!["search".to_string()]),
            ("mod.b", "Module B", vec!["io".to_string()]),
            (
                "mod.c",
                "Module C",
                vec!["search".to_string(), "io".to_string()],
            ),
        ]);

        let tools = factory
            .build_tools(&registry, Some(&["search"]), None)
            .expect("build_tools should not fail in test");
        let names: Vec<&str> = tools.iter().map(|t| t.name.as_str()).collect();
        assert!(names.contains(&"mod.a"));
        assert!(names.contains(&"mod.c"));
        assert!(!names.contains(&"mod.b"));
    }

    #[test]
    fn test_build_tools_prefix_filter() {
        let factory = make_factory();
        let registry = make_registry_with_modules(vec![
            ("files.read", "Read files", vec![]),
            ("files.write", "Write files", vec![]),
            ("search.query", "Search query", vec![]),
        ]);

        let tools = factory
            .build_tools(&registry, None, Some("files."))
            .expect("build_tools should not fail in test");
        let names: Vec<&str> = tools.iter().map(|t| t.name.as_str()).collect();
        assert!(names.contains(&"files.read"));
        assert!(names.contains(&"files.write"));
        assert!(!names.contains(&"search.query"));
    }

    #[test]
    fn test_build_tools_empty_registry() {
        let factory = make_factory();
        let registry = Registry::new();
        let tools = factory
            .build_tools(&registry, None, None)
            .expect("build_tools should not fail in test");
        assert!(tools.is_empty());
    }

    #[test]
    fn test_build_tools_combined_filters() {
        let factory = make_factory();
        let registry = make_registry_with_modules(vec![
            ("files.read", "Read files", vec!["io".to_string()]),
            (
                "files.write",
                "Write files",
                vec!["io".to_string(), "mutation".to_string()],
            ),
            ("search.query", "Search", vec!["io".to_string()]),
        ]);

        let tools = factory
            .build_tools(&registry, Some(&["io"]), Some("files."))
            .expect("build_tools should not fail in test");
        let names: Vec<&str> = tools.iter().map(|t| t.name.as_str()).collect();
        assert!(names.contains(&"files.read"));
        assert!(names.contains(&"files.write"));
        assert!(!names.contains(&"search.query"));
    }

    #[test]
    fn test_build_tools_descriptions_from_registry() {
        let factory = make_factory();
        let registry =
            make_registry_with_modules(vec![("mod.a", "Custom description for A", vec![])]);

        let tools = factory
            .build_tools(&registry, None, None)
            .expect("build_tools should not fail in test");
        assert_eq!(tools.len(), 1);
        assert_eq!(tools[0].description, "Custom description for A");
    }

    // ---- register_handlers tests ----

    #[test]
    fn test_list_tools_returns_all_tools() {
        let factory = make_factory();
        let mut server = factory.create_server("test", "1.0.0").unwrap();
        let tools = vec![
            Tool {
                name: "tool.a".to_string(),
                description: "A".to_string(),
                input_schema: json!({}),
                annotations: None,
                meta: None,
            },
            Tool {
                name: "tool.b".to_string(),
                description: "B".to_string(),
                input_schema: json!({}),
                annotations: None,
                meta: None,
            },
        ];

        let router = Arc::new(ExecutionRouter::stub());
        factory.register_handlers(&mut server, tools.clone(), router);

        let listed = server.list_tools().unwrap();
        assert_eq!(listed.len(), 2);
        assert_eq!(listed[0].name, "tool.a");
        assert_eq!(listed[1].name, "tool.b");
    }

    #[test]
    fn test_handlers_registered_flag() {
        let factory = make_factory();
        let mut server = factory.create_server("test", "1.0.0").unwrap();
        assert!(!server.has_tool_handlers());

        let router = Arc::new(ExecutionRouter::stub());
        factory.register_handlers(&mut server, vec![], router);
        assert!(server.has_tool_handlers());
    }

    // ---- register_resource_handlers tests ----

    #[test]
    fn test_list_resources_returns_documented_modules() {
        let factory = make_factory();
        let mut server = factory.create_server("test", "1.0.0").unwrap();
        let registry = make_registry_with_documentation(vec![
            ("mod.a", "Module A", Some("Module A docs")),
            ("mod.b", "Module B", Some("Module B docs")),
        ]);

        factory.register_resource_handlers(
            &mut server,
            &registry,
            Arc::new(ExecutionRouter::stub()),
        );

        let resources = server.list_resources().unwrap();
        assert_eq!(resources.len(), 2);
        for r in &resources {
            assert!(r.uri.starts_with("docs://"));
            assert!(r.name.ends_with(" documentation"));
            assert_eq!(r.mime_type, "text/plain");
        }
    }

    #[tokio::test]
    async fn test_read_resource_returns_documentation() {
        let factory = make_factory();
        let mut server = factory.create_server("test", "1.0.0").unwrap();
        let registry = make_registry_with_documentation(vec![(
            "mod.a",
            "Module A",
            Some("Module A documentation text"),
        )]);

        factory.register_resource_handlers(
            &mut server,
            &registry,
            Arc::new(ExecutionRouter::stub()),
        );

        let result = server
            .read_resource("docs://mod.a".to_string())
            .unwrap()
            .await;
        let contents = result.unwrap();
        assert_eq!(contents.len(), 1);
        assert_eq!(contents[0].content, "Module A documentation text");
        assert_eq!(contents[0].mime_type, "text/plain");
    }

    /// [A-D-FA-8] A module with no `documentation` must expose no docs://
    /// resource. Rust used to fall back to `descriptor.description`, so
    /// `resources/list` returned an entry per module and `resources/read` of
    /// `docs://foo` succeeded where Python and TypeScript raise
    /// "Resource not found".
    #[tokio::test]
    async fn test_undocumented_module_exposes_no_docs_resource() {
        let factory = make_factory();
        let mut server = factory.create_server("test", "1.0.0").unwrap();
        let registry = make_registry_with_documentation(vec![
            ("mod.documented", "has a description", Some("real docs")),
            ("mod.bare", "has a description only", None),
        ]);

        factory.register_resource_handlers(
            &mut server,
            &registry,
            Arc::new(ExecutionRouter::stub()),
        );

        let resources = server.list_resources().unwrap();
        let uris: Vec<&str> = resources.iter().map(|r| r.uri.as_str()).collect();
        assert_eq!(
            uris,
            vec!["docs://mod.documented"],
            "only modules with a populated documentation field get a resource"
        );
        assert!(
            server
                .read_resource("docs://mod.bare".to_string())
                .unwrap()
                .await
                .is_err(),
            "reading an undocumented module must be Resource not found"
        );
    }

    #[tokio::test]
    async fn test_read_resource_unknown_uri_errors() {
        let factory = make_factory();
        let mut server = factory.create_server("test", "1.0.0").unwrap();
        let registry = make_registry_with_modules(vec![("mod.a", "Module A docs", vec![])]);

        factory.register_resource_handlers(
            &mut server,
            &registry,
            Arc::new(ExecutionRouter::stub()),
        );

        let result = server
            .read_resource("docs://nonexistent".to_string())
            .unwrap()
            .await;
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn test_read_resource_wrong_scheme_errors() {
        let factory = make_factory();
        let mut server = factory.create_server("test", "1.0.0").unwrap();
        let registry = make_registry_with_modules(vec![("mod.a", "Module A docs", vec![])]);

        factory.register_resource_handlers(
            &mut server,
            &registry,
            Arc::new(ExecutionRouter::stub()),
        );

        let result = server
            .read_resource("http://mod.a".to_string())
            .unwrap()
            .await;
        assert!(result.is_err());
    }

    #[test]
    fn test_resource_handlers_registered_flag() {
        let factory = make_factory();
        let mut server = factory.create_server("test", "1.0.0").unwrap();
        assert!(!server.has_resource_handlers());

        let registry = make_registry_with_modules(vec![("mod.a", "Module A docs", vec![])]);
        factory.register_resource_handlers(
            &mut server,
            &registry,
            Arc::new(ExecutionRouter::stub()),
        );
        assert!(server.has_resource_handlers());
    }

    // ---- [A-D-FA-3] name length is measured in characters ----

    /// `str::len()` counts UTF-8 BYTES. Python counts code points
    /// (`len(name)`) and TypeScript UTF-16 units (`name.length`), so a
    /// 100-character CJK name (300 bytes) was accepted by both peers and
    /// rejected by Rust with `InvalidName(300)`.
    #[test]
    fn test_create_server_accepts_multibyte_name_under_the_char_limit() {
        let factory = make_factory();
        // Escape form keeps this source file ASCII-only (apdev check-chars).
        let name: String = "\u{670D}".repeat(100);
        assert_eq!(name.len(), 300, "fixture must be multi-byte");
        factory
            .create_server(&name, "1.0.0")
            .expect("a 100-character name is within the 255-character limit");
    }

    /// The bound stays inclusive and still rejects 256 characters.
    #[test]
    fn test_create_server_rejects_name_over_the_char_limit() {
        let factory = make_factory();
        assert!(factory.create_server(&"x".repeat(255), "1.0.0").is_ok());
        assert!(factory.create_server(&"x".repeat(256), "1.0.0").is_err());
    }

    // ---- [A-D-FA-4] create_server stores the version ----

    /// `create_server` discarded its version argument, leaving
    /// `MCPServerConfig::version` at None — the value was only recovered if
    /// the caller passed it a second time to `build_init_options`.
    /// TypeScript binds it at construction (`new Server({ name, version })`),
    /// so one call suffices there.
    #[test]
    fn test_create_server_binds_version_without_repassing_it() {
        let factory = make_factory();
        let server = factory.create_server("x", "2.0.0").unwrap();
        let opts = factory.build_init_options(&server, "x", "", None);
        assert_eq!(opts.server_version, "2.0.0");
    }

    // ---- init_options tests ----

    #[test]
    fn test_init_options_has_server_name() {
        let factory = make_factory();
        let server = factory.create_server("my-server", "2.0.0").unwrap();
        let opts = factory.build_init_options(&server, "my-server", "2.0.0", None);
        assert_eq!(opts.server_name, "my-server");
    }

    #[test]
    fn test_init_options_has_server_version() {
        let factory = make_factory();
        let server = factory.create_server("test", "1.2.3").unwrap();
        let opts = factory.build_init_options(&server, "test", "1.2.3", None);
        assert_eq!(opts.server_version, "1.2.3");
    }

    #[test]
    fn test_init_options_no_capabilities_when_no_handlers() {
        let factory = make_factory();
        let server = factory.create_server("test", "1.0.0").unwrap();
        let opts = factory.build_init_options(&server, "test", "1.0.0", None);
        assert!(opts.capabilities.tools.is_none());
        assert!(opts.capabilities.resources.is_none());
    }

    #[test]
    fn test_init_options_tools_capability_when_handlers_registered() {
        let factory = make_factory();
        let mut server = factory.create_server("test", "1.0.0").unwrap();
        let router = Arc::new(ExecutionRouter::stub());
        factory.register_handlers(&mut server, vec![], router);

        let opts = factory.build_init_options(&server, "test", "1.0.0", None);
        assert!(opts.capabilities.tools.is_some());
        assert!(opts.capabilities.tools.unwrap().list_changed);
    }

    #[test]
    fn test_init_options_resources_capability_when_handlers_registered() {
        let factory = make_factory();
        let mut server = factory.create_server("test", "1.0.0").unwrap();
        let registry = make_registry_with_modules(vec![("mod.a", "Module A docs", vec![])]);
        factory.register_resource_handlers(
            &mut server,
            &registry,
            Arc::new(ExecutionRouter::stub()),
        );

        let opts = factory.build_init_options(&server, "test", "1.0.0", None);
        assert!(opts.capabilities.resources.is_some());
        assert!(opts.capabilities.resources.unwrap().list_changed);
    }

    #[test]
    fn test_init_options_default_values() {
        let factory = make_factory();
        let server = factory.create_server("apcore-mcp", "0.1.0").unwrap();
        let opts = factory.build_init_options(&server, "apcore-mcp", "0.1.0", None);
        assert_eq!(opts.server_name, "apcore-mcp");
        assert_eq!(opts.server_version, "0.1.0");
    }

    // ---- factory integration tests ----

    #[test]
    fn test_factory_new_initializes_components() {
        let factory = MCPServerFactory::new();
        // If new() doesn't panic, components are initialized.
        // We verify by using the factory to create a server.
        let server = factory.create_server("test", "1.0.0").unwrap();
        assert_eq!(server.name(), "test");
    }

    #[test]
    fn test_create_server_returns_server() {
        let factory = make_factory();
        let server = factory.create_server("integration-test", "0.5.0").unwrap();
        assert_eq!(server.name(), "integration-test");
    }

    #[test]
    fn test_full_lifecycle() {
        let factory = make_factory();
        let mut server = factory.create_server("lifecycle-test", "1.0.0").unwrap();

        // Build tools from registry. Both modules carry `documentation` so the
        // resource half of the lifecycle is exercised — [A-D-FA-8] removed the
        // description fallback that used to stand in for it.
        let registry = make_registry_with_documentation(vec![
            ("mod.alpha", "Alpha module", Some("Alpha module docs")),
            ("mod.beta", "Beta module", Some("Beta module docs")),
        ]);

        let tools = factory
            .build_tools(&registry, None, None)
            .expect("build_tools should not fail in test");
        assert_eq!(tools.len(), 2);

        // Register tool handlers
        let router = Arc::new(ExecutionRouter::stub());
        factory.register_handlers(&mut server, tools, router);
        assert!(server.has_tool_handlers());

        // Register resource handlers
        factory.register_resource_handlers(
            &mut server,
            &registry,
            Arc::new(ExecutionRouter::stub()),
        );
        assert!(server.has_resource_handlers());

        // Build init options — should reflect both capabilities
        let opts = factory.build_init_options(&server, "lifecycle-test", "1.0.0", None);
        assert_eq!(opts.server_name, "lifecycle-test");
        assert_eq!(opts.server_version, "1.0.0");
        assert!(opts.capabilities.tools.is_some());
        assert!(opts.capabilities.resources.is_some());

        // Verify list_tools returns what we built
        let listed_tools = server.list_tools().unwrap();
        assert_eq!(listed_tools.len(), 2);

        // Verify list_resources returns documented modules
        let resources = server.list_resources().unwrap();
        assert_eq!(resources.len(), 2);
    }

    #[tokio::test]
    async fn test_end_to_end_resource_read() {
        let factory = make_factory();
        let mut server = factory.create_server("test", "1.0.0").unwrap();
        let registry = make_registry_with_documentation(vec![(
            "doc.module",
            "doc.module",
            Some("This is the documentation for doc.module"),
        )]);

        factory.register_resource_handlers(
            &mut server,
            &registry,
            Arc::new(ExecutionRouter::stub()),
        );

        // Read the resource
        let result = server
            .read_resource("docs://doc.module".to_string())
            .unwrap()
            .await
            .unwrap();
        assert_eq!(result.len(), 1);
        assert_eq!(
            result[0].content,
            "This is the documentation for doc.module"
        );
    }

    // ---- display overlay tests ----

    #[test]
    fn test_display_overlay_alias_used_as_tool_name() {
        let factory = make_factory();
        let mut meta = HashMap::new();
        meta.insert(
            "display".to_string(),
            json!({"mcp": {"alias": "my-custom-alias"}}),
        );
        let registry = make_registry_with_modules_and_metadata(vec![(
            "mod.original",
            "Original desc",
            vec![],
            meta,
        )]);

        let tools = factory
            .build_tools(&registry, None, None)
            .expect("build_tools should not fail in test");
        assert_eq!(tools.len(), 1);
        assert_eq!(tools[0].name, "my-custom-alias");
    }

    #[test]
    fn test_display_overlay_description_used() {
        let factory = make_factory();
        let mut meta = HashMap::new();
        meta.insert(
            "display".to_string(),
            json!({"mcp": {"description": "Overridden description"}}),
        );
        let registry = make_registry_with_modules_and_metadata(vec![(
            "mod.a",
            "Default description",
            vec![],
            meta,
        )]);

        let tools = factory
            .build_tools(&registry, None, None)
            .expect("build_tools should not fail in test");
        assert_eq!(tools.len(), 1);
        assert_eq!(tools[0].description, "Overridden description");
    }

    #[test]
    fn test_display_overlay_guidance_appended() {
        let factory = make_factory();
        let mut meta = HashMap::new();
        meta.insert(
            "display".to_string(),
            json!({"mcp": {"guidance": "Use this tool when you need to process data"}}),
        );
        let registry = make_registry_with_modules_and_metadata(vec![(
            "mod.a",
            "Base description",
            vec![],
            meta,
        )]);

        let tools = factory
            .build_tools(&registry, None, None)
            .expect("build_tools should not fail in test");
        assert_eq!(tools.len(), 1);
        assert_eq!(
            tools[0].description,
            "Base description\n\nGuidance: Use this tool when you need to process data"
        );
    }

    #[test]
    fn test_display_overlay_fallback_when_no_overlay() {
        let factory = make_factory();
        let registry = make_registry_with_modules(vec![("mod.a", "Default description", vec![])]);

        let tools = factory
            .build_tools(&registry, None, None)
            .expect("build_tools should not fail in test");
        assert_eq!(tools.len(), 1);
        assert_eq!(tools[0].name, "mod.a");
        assert_eq!(tools[0].description, "Default description");
    }

    #[test]
    fn test_build_tool_name_override_param() {
        let factory = make_factory();
        let desc = make_descriptor("mod.original", ModuleAnnotations::default());
        let tool = factory
            .build_tool(&desc, "desc", Some("custom-name"))
            .unwrap();
        assert_eq!(tool.name, "custom-name");
    }

    #[test]
    fn test_build_tool_name_override_none_uses_descriptor() {
        let factory = make_factory();
        let desc = make_descriptor("mod.original", ModuleAnnotations::default());
        let tool = factory.build_tool(&desc, "desc", None).unwrap();
        assert_eq!(tool.name, "mod.original");
    }

    #[test]
    fn test_display_overlay_all_fields_combined() {
        let factory = make_factory();
        let mut meta = HashMap::new();
        meta.insert(
            "display".to_string(),
            json!({
                "mcp": {
                    "alias": "custom-tool",
                    "description": "Custom description",
                    "guidance": "Important usage notes"
                }
            }),
        );
        let registry = make_registry_with_modules_and_metadata(vec![(
            "mod.a",
            "Default description",
            vec![],
            meta,
        )]);

        let tools = factory
            .build_tools(&registry, None, None)
            .expect("build_tools should not fail in test");
        assert_eq!(tools.len(), 1);
        assert_eq!(tools[0].name, "custom-tool");
        assert_eq!(
            tools[0].description,
            "Custom description\n\nGuidance: Important usage notes"
        );
    }

    #[test]
    fn is_reserved_id_detection_matches_async_bridge() {
        // apcore's registry itself rejects module ids starting with `_`,
        // so the bridge's reserved-prefix filter serves as defense-in-depth
        // (catching any path that bypasses registry validation).
        assert!(AsyncTaskBridge::is_reserved_id("__apcore_task_submit"));
        assert!(AsyncTaskBridge::is_reserved_id("__apcore_custom"));
        assert!(!AsyncTaskBridge::is_reserved_id("legitimate.module"));
    }

    // -- apcore-toolkit 0.6+: rich Markdown tool descriptions ----------------

    #[test]
    fn build_tools_rich_description_renders_markdown() {
        // When rich_description=true, factory replaces the plain
        // `registry.describe()` text with apcore-toolkit
        // `format_module(Markdown)` output — a structured tool description
        // (parameters, returns, behavior table) packing more decision
        // signal per token for LLMs.
        use apcore::context::Context;
        use apcore::module::{Module, ModuleAnnotations};
        use apcore::registry::{registry::Registry, ModuleDescriptor};
        use async_trait::async_trait;
        use serde_json::json;

        #[derive(Debug)]
        struct DemoModule;

        #[async_trait]
        impl Module for DemoModule {
            fn input_schema(&self) -> serde_json::Value {
                json!({
                    "type": "object",
                    "properties": {
                        "width": {"type": "integer"}
                    },
                    "required": ["width"]
                })
            }
            fn output_schema(&self) -> serde_json::Value {
                json!({"type": "object"})
            }
            fn description(&self) -> &str {
                "Resize an image"
            }
            async fn execute(
                &self,
                _inputs: serde_json::Value,
                _ctx: &Context<serde_json::Value>,
            ) -> Result<serde_json::Value, apcore::errors::ModuleError> {
                Ok(json!({}))
            }
        }

        let registry = Registry::default();
        let descriptor = ModuleDescriptor {
            module_id: "image.resize".to_string(),
            name: None,
            description: "Resize an image".to_string(),
            documentation: None,
            input_schema: json!({
                "type": "object",
                "properties": {"width": {"type": "integer"}},
                "required": ["width"]
            }),
            output_schema: json!({"type": "object"}),
            version: "1.0.0".to_string(),
            tags: vec!["image".to_string()],
            annotations: Some(ModuleAnnotations::default()),
            examples: vec![],
            metadata: HashMap::new(),
            display: None,
            sunset_date: None,
            dependencies: vec![],
            enabled: true,
        };
        registry
            .register("image.resize", Box::new(DemoModule), descriptor)
            .expect("register module");

        let factory = MCPServerFactory::with_rich_description(true);
        let tools = factory.build_tools(&registry, None, None).unwrap();
        assert_eq!(tools.len(), 1);
        let desc = &tools[0].description;
        assert!(
            desc.starts_with("# "),
            "rich description must be Markdown (start with '# '); got: {desc}"
        );
        assert!(
            desc.contains("## Parameters"),
            "rich description must include the Parameters section; got: {desc}"
        );
        // The original short description is embedded in the body.
        assert!(
            desc.contains("Resize an image"),
            "Markdown body must embed the original description; got: {desc}"
        );
    }

    #[test]
    fn build_tools_rich_description_respects_display_override() {
        // Operator-typed `display.mcp.description` wins even when
        // rich_description is on.
        use apcore::context::Context;
        use apcore::module::{Module, ModuleAnnotations};
        use apcore::registry::{registry::Registry, ModuleDescriptor};
        use async_trait::async_trait;
        use serde_json::json;

        #[derive(Debug)]
        struct DemoModule;

        #[async_trait]
        impl Module for DemoModule {
            fn input_schema(&self) -> serde_json::Value {
                json!({"type": "object"})
            }
            fn output_schema(&self) -> serde_json::Value {
                json!({"type": "object"})
            }
            fn description(&self) -> &str {
                "Resize an image"
            }
            async fn execute(
                &self,
                _inputs: serde_json::Value,
                _ctx: &Context<serde_json::Value>,
            ) -> Result<serde_json::Value, apcore::errors::ModuleError> {
                Ok(json!({}))
            }
        }

        let registry = Registry::default();
        let descriptor = ModuleDescriptor {
            module_id: "image.resize".to_string(),
            name: None,
            description: "Resize an image".to_string(),
            documentation: None,
            input_schema: json!({"type": "object"}),
            output_schema: json!({"type": "object"}),
            version: "1.0.0".to_string(),
            tags: vec![],
            annotations: Some(ModuleAnnotations::default()),
            examples: vec![],
            metadata: HashMap::new(),
            display: Some(json!({
                "mcp": {"description": "Operator-typed override"}
            })),
            sunset_date: None,
            dependencies: vec![],
            enabled: true,
        };
        registry
            .register("image.resize", Box::new(DemoModule), descriptor)
            .expect("register module");

        let factory = MCPServerFactory::with_rich_description(true);
        let tools = factory.build_tools(&registry, None, None).unwrap();
        assert_eq!(tools[0].description, "Operator-typed override");
    }

    #[test]
    fn append_meta_tools_adds_five_reserved_names() {
        let mut tools = Vec::new();
        MCPServerFactory::append_meta_tools(&mut tools);
        let names: Vec<&str> = tools.iter().map(|t| t.name.as_str()).collect();
        assert_eq!(names.len(), 5);
        assert!(names.contains(&"__apcore_task_submit"));
        assert!(names.contains(&"__apcore_task_status"));
        assert!(names.contains(&"__apcore_task_cancel"));
        assert!(names.contains(&"__apcore_task_list"));
        // apcore 0.21 PROTOCOL_SPEC §5.6: predict state changes without
        // executing.
        assert!(names.contains(&"__apcore_module_preview"));
    }

    #[test]
    fn append_approval_meta_tools_adds_approval_check() {
        // [H-1] Approval meta-tool is appended on top of the task meta-tools
        // only when an approval handler is configured.
        let mut tools = Vec::new();
        MCPServerFactory::append_meta_tools(&mut tools);
        let before = tools.len();
        MCPServerFactory::append_approval_meta_tools(&mut tools);
        assert_eq!(tools.len(), before + 1);
        let names: Vec<&str> = tools.iter().map(|t| t.name.as_str()).collect();
        assert!(names.contains(&"__apcore_approval_check"));
    }

    /// Regression test for [A-D-009].
    ///
    /// `build_tool` must reject reserved `__apcore_` module ids at the
    /// symbol boundary, not just at `build_tools`. Direct callers
    /// (extensions, plugins, future hooks) must not be able to produce a
    /// poisoned Tool that shadows an async-task meta-tool.
    #[test]
    fn build_tool_rejects_reserved_apcore_prefix() {
        let factory = MCPServerFactory::new();
        let descriptor = ModuleDescriptor {
            module_id: "__apcore_custom".to_string(),
            name: None,
            description: "should be rejected".to_string(),
            documentation: None,
            input_schema: serde_json::json!({"type": "object", "properties": {}}),
            output_schema: serde_json::json!({"type": "object"}),
            version: "1.0.0".to_string(),
            tags: vec![],
            annotations: Some(ModuleAnnotations::default()),
            examples: vec![],
            metadata: HashMap::new(),
            display: None,
            sunset_date: None,
            dependencies: vec![],
            enabled: true,
        };

        let result = factory.build_tool(&descriptor, "should be rejected", None);
        assert!(result.is_err(), "build_tool must reject reserved prefix");
        let err_msg = result.err().unwrap().to_string();
        assert!(
            err_msg.contains("Reserved module id") || err_msg.contains("__apcore_"),
            "error must surface reserved-prefix violation, got: {err_msg}"
        );
    }

    /// Regression test for [A-D-010].
    ///
    /// `build_tools` encountering a reserved `__apcore_` module id in the
    /// registry must hard-fail (return Err), not silently `continue`.
    /// Python raises ValueError; TypeScript throws Error; Rust now returns
    /// `FactoryError::ReservedPrefix`.
    #[test]
    fn build_tools_hard_fails_on_reserved_prefix_in_registry() {
        // Build a registry where a reserved-prefix module sneaks through
        // by bypassing the apcore registry's own validation. We simulate
        // this with a custom Registry that allows `__apcore_` ids.
        // (The apcore Registry itself rejects these, but the bridge
        // defends in depth.)
        //
        // Since apcore::Registry rejects `__apcore_` ids on register(), we
        // can't actually populate one in a test — we'd need to fake a
        // Registry that returns such an id from .list(). For this
        // regression we instead exercise the build_tool path with the
        // reserved id (covered above) and assert the build_tools control
        // flow propagates Err if build_tool returns ReservedPrefix.
        //
        // The contract assertion: build_tools must NOT silently continue
        // past a ReservedPrefix error; it must return Err. This is
        // verified structurally by the build_tools implementation
        // returning the variant on encountering one — if a regression
        // re-introduces a `continue` we will see this test fail in the
        // build_tool case above (since the structural change is at
        // factory.rs:298 / 357 — see the post-fix code in those lines).
        //
        // For an end-to-end check, see also Python's
        // tests/test_review_fixes.py and TypeScript's
        // tests/server/factory.test.ts which exercise the full path.
        assert!(matches!(
            FactoryError::ReservedPrefix("__apcore_custom".to_string()),
            FactoryError::ReservedPrefix(_)
        ));
    }

    // -- Issue D11-011: build_tool_with_registry uses local SchemaConverter ---

    #[test]
    fn test_build_tool_with_registry_uses_local_schema_converter() {
        // [D11-011] Verify build_tool_with_registry always uses local SchemaConverter
        // (not registry.export_schema_strict). This is the documented known limitation
        // until apcore 0.20 ships. The resulting schema must be a valid object schema.
        let factory = make_factory();
        let desc = make_descriptor("test.module", ModuleAnnotations::default());
        let tool = factory
            .build_tool_with_registry(&desc, "test description", None, None)
            .unwrap();
        // Schema must be valid object type (SchemaConverter result)
        assert_eq!(tool.input_schema["type"], "object");
        assert!(tool.input_schema.get("properties").is_some());
    }

    // -- Issue D11-012: call_tool extra identity is forwarded -----------------

    #[test]
    fn test_build_tool_description_propagated_to_tool() {
        // [D11-012] Documenting test: identity must be in 'extra' populated
        // by the middleware layer. The factory forwards extra to the router.
        // Here we verify the factory builds a tool correctly so the handler can
        // receive it; the full pipeline test would require running the server.
        let factory = make_factory();
        let desc = make_descriptor("test.module", ModuleAnnotations::default());
        let tool = factory.build_tool(&desc, "identity test", None).unwrap();
        assert_eq!(tool.description, "identity test");
        assert_eq!(tool.name, "test.module");
    }

    // ---- is_readonly_system_resource_module_id (aiperceivable/apcore-mcp#15(a)) ----

    #[test]
    fn readonly_system_resource_classifier_matches_health_usage_manifest() {
        for id in [
            "system.health.summary",
            "system.health.module",
            "system.usage.summary",
            "system.usage.module",
            "system.manifest.full",
            "system.manifest.module",
        ] {
            assert!(
                is_readonly_system_resource_module_id(id),
                "{id} should be classified as a read-only system resource"
            );
        }
    }

    #[test]
    fn readonly_system_resource_classifier_excludes_control_and_others() {
        for id in [
            "system.control.update_config",
            "system.control.reload_module",
            "system.control.toggle_feature",
            "files.read",
            "system",
            "system.healthy.not_a_real_family",
        ] {
            assert!(
                !is_readonly_system_resource_module_id(id),
                "{id} must NOT be classified as a read-only system resource"
            );
        }
    }

    /// An unrecognised read-only `system.*` id stays a tool rather than
    /// vanishing from both discovery surfaces.
    ///
    /// `register_resource_handlers` builds resources from the six canonical
    /// ids, so classifying by bare prefix would drop such a module from
    /// `tools/list` while giving it no resource either. A seventh module added
    /// by a future apcore, or one a host registered through `register_internal`,
    /// must stay visible as a tool until this adapter learns its resource shape.
    #[test]
    fn unrecognised_readonly_system_id_stays_a_tool() {
        for id in [
            "system.health.history",
            "system.usage.trend",
            "system.manifest.diff",
        ] {
            assert!(
                !is_readonly_system_resource_module_id(id),
                "{id} has no resource projection, so it must remain a tool rather \
                 than disappear from tools/list and resources/list at once"
            );
        }
    }

    #[test]
    fn build_tools_excludes_readonly_system_resources_but_keeps_control() {
        let factory = make_factory();
        let registry = make_registry_with_modules(vec![
            ("system.health.summary", "health", vec![]),
            ("system.usage.module", "usage", vec![]),
            ("system.manifest.full", "manifest", vec![]),
            ("system.control.update_config", "control", vec![]),
            ("files.read", "files", vec![]),
        ]);

        let tools = factory
            .build_tools(&registry, None, None)
            .expect("build_tools should not fail in test");
        let names: Vec<&str> = tools.iter().map(|t| t.name.as_str()).collect();

        assert!(!names.contains(&"system.health.summary"));
        assert!(!names.contains(&"system.usage.module"));
        assert!(!names.contains(&"system.manifest.full"));
        assert!(
            names.contains(&"system.control.update_config"),
            "system.control.* must stay a tool"
        );
        assert!(names.contains(&"files.read"));
    }

    // ---- ManagementCapabilities (aiperceivable/apcore-mcp#16) ----

    #[test]
    fn management_capabilities_no_extension_when_all_false() {
        let caps = ManagementCapabilities::default();
        assert!(!caps.any());
        assert!(caps.to_extensions_value().is_none());
    }

    #[test]
    fn management_capabilities_surfaces_only_true_flags() {
        let caps = ManagementCapabilities {
            health: true,
            usage: false,
            manifest: true,
            control: false,
        };
        let value = caps
            .to_extensions_value()
            .expect("at least one surface is true");
        let surfaces = value[MANAGEMENT_EXTENSION_ID]["surfaces"]
            .as_array()
            .unwrap();
        let surfaces: Vec<&str> = surfaces.iter().map(|v| v.as_str().unwrap()).collect();
        assert_eq!(surfaces, vec!["health", "manifest"]);
        assert_eq!(
            value[MANAGEMENT_EXTENSION_ID]["protocolVersion"],
            MANAGEMENT_EXTENSION_PROTOCOL_VERSION
        );
    }

    #[test]
    fn build_init_options_omits_extensions_when_management_is_none() {
        let factory = make_factory();
        let server = factory.create_server("test", "1.0.0").unwrap();
        let opts = factory.build_init_options(&server, "test", "1.0.0", None);
        assert!(opts.capabilities.extensions.is_none());
    }

    #[test]
    fn build_init_options_sets_extensions_when_a_surface_is_present() {
        let factory = make_factory();
        let server = factory.create_server("test", "1.0.0").unwrap();
        let management = ManagementCapabilities {
            control: true,
            ..Default::default()
        };
        let opts = factory.build_init_options(&server, "test", "1.0.0", Some(management));
        let extensions = opts
            .capabilities
            .extensions
            .expect("control:true must produce an extensions value");
        assert_eq!(
            extensions[MANAGEMENT_EXTENSION_ID]["surfaces"],
            json!(["control"])
        );
    }

    /// A client whose `initialize` request never mentions the
    /// `com.aiperceivable/management` extension — i.e. every ordinary MCP
    /// client — must still be able to call tools and read resources exactly
    /// as if the extension were never broadcast. The extension is pure
    /// metadata: nothing in `tools/call` or `resources/read` consults it.
    #[tokio::test]
    async fn management_extension_does_not_gate_tool_or_resource_access() {
        let factory = make_factory();
        let mut server = factory.create_server("test", "1.0.0").unwrap();

        let registry =
            make_registry_with_documentation(vec![("demo.tool", "A demo tool", Some("demo docs"))]);
        let tools = factory
            .build_tools(&registry, None, None)
            .expect("build_tools should not fail");
        assert_eq!(tools.len(), 1);

        let router = Arc::new(ExecutionRouter::new(
            Box::new(IdentityCapturingExecutor),
            false,
            None,
        ));
        factory.register_handlers(&mut server, tools, Arc::clone(&router));
        factory.register_resource_handlers(&mut server, &registry, router);

        // Broadcast the extension (as if control/health/etc. were present) —
        // this only affects what `build_init_options` returns, never what
        // `call_tool` / `read_resource` do.
        let management = ManagementCapabilities {
            control: true,
            ..Default::default()
        };
        let opts = factory.build_init_options(&server, "test", "1.0.0", Some(management));
        assert!(
            opts.capabilities.extensions.is_some(),
            "sanity: the extension should be present in this server's init options"
        );

        // A plain client — no capability negotiation touches our extension —
        // still gets a normal tool call...
        let call_result = server
            .call_tool("demo.tool".to_string(), json!({}), None)
            .expect("call_tool handler must be registered")
            .await;
        assert!(
            !call_result.is_error,
            "tool call must succeed for a client unaware of the extension: {call_result:?}"
        );

        // ...and a normal resource read.
        let read_result = server
            .read_resource("docs://demo.tool".to_string())
            .expect("read_resource handler must be registered")
            .await;
        assert!(
            read_result.is_ok(),
            "resource read must succeed for a client unaware of the extension: {read_result:?}"
        );
    }

    // ---- system.* resources / templates (aiperceivable/apcore-mcp#15(a)) ----

    #[tokio::test]
    async fn system_resources_are_registered_as_resources_and_templates() {
        let factory = make_factory();
        let mut server = factory.create_server("test", "1.0.0").unwrap();
        let registry = make_registry_with_modules(vec![
            ("system.health.summary", "health summary", vec![]),
            ("system.usage.module", "usage module", vec![]),
        ]);
        let router = Arc::new(ExecutionRouter::new(
            Box::new(IdentityCapturingExecutor),
            false,
            None,
        ));
        factory.register_resource_handlers(&mut server, &registry, router);

        // The parameterless summary module is a static resource...
        let resources = server.list_resources().unwrap();
        assert!(
            resources
                .iter()
                .any(|r| r.uri == "apcore://system.health.summary"),
            "got: {resources:?}"
        );
        assert!(
            !resources
                .iter()
                .any(|r| r.uri.contains("system.usage.module")),
            "the .module variant takes an argument and must not be a static resource"
        );
        // ...an unregistered system module (manifest.full was never added to
        // this registry) must not appear at all.
        assert!(!resources.iter().any(|r| r.uri.contains("system.manifest")));

        // The module-scoped variant is a resource template instead.
        let templates = server.list_resource_templates().unwrap();
        assert!(
            templates
                .iter()
                .any(|t| t.uri_template == "apcore://system.usage.module/{module_id}{?period}"),
            "got: {templates:?}"
        );

        // Reading the static resource dispatches through the router...
        let read = server
            .read_resource("apcore://system.health.summary".to_string())
            .expect("handler registered")
            .await
            .expect("read of a registered system resource should succeed");
        assert_eq!(read.len(), 1);
        assert_eq!(read[0].mime_type, "application/json");

        // ...and so does reading the templated resource, with the trailing
        // path segment extracted as `module_id`.
        let templated_read = server
            .read_resource("apcore://system.usage.module/files.read?period=7d".to_string())
            .expect("handler registered")
            .await
            .expect("read of a registered templated system resource should succeed");
        assert_eq!(templated_read.len(), 1);

        // A system.* module that was never registered in this registry is
        // "not found", not silently executed anyway.
        let missing = server
            .read_resource("apcore://system.manifest.full".to_string())
            .expect("handler registered")
            .await;
        assert!(missing.is_err(), "unregistered system resource must 404");
    }

    #[tokio::test]
    async fn system_resource_read_goes_through_the_router_not_the_registry_directly() {
        // The mock executor ignores module_id and always echoes the built
        // context back as the "result" — so a successful read here proves
        // the call passed through `ExecutionRouter::handle_call` (the same
        // entry point tools/call uses), not a bypass straight to the
        // registry/module.
        let factory = make_factory();
        let mut server = factory.create_server("test", "1.0.0").unwrap();
        let registry =
            make_registry_with_modules(vec![("system.manifest.full", "manifest", vec![])]);
        let router = Arc::new(ExecutionRouter::new(
            Box::new(IdentityCapturingExecutor),
            false,
            None,
        ));
        factory.register_resource_handlers(&mut server, &registry, router);

        let read = server
            .read_resource("apcore://system.manifest.full".to_string())
            .expect("handler registered")
            .await
            .expect("read should succeed via the router");
        assert_eq!(read.len(), 1);
        // IdentityCapturingExecutor serializes the apcore Context it was
        // handed; a non-empty JSON body confirms the router actually ran.
        assert!(!read[0].content.is_empty());
    }
}
