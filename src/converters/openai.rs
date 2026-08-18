//! OpenAIConverter — converts apcore module descriptors to OpenAI function-calling format.

use std::collections::HashMap;

use apcore::module::{ModuleAnnotations, ModuleExample};
use apcore::registry::registry::Registry;
use apcore_toolkit::ScannedModule;
use serde_json::Value;

use crate::adapters::{AdapterError, AnnotationMapper, ModuleIDNormalizer, SchemaConverter};
use crate::markdown;

// ---- Error type -------------------------------------------------------------

/// Errors that can occur during OpenAI tool conversion.
#[derive(Debug, thiserror::Error)]
pub enum ConverterError {
    /// An adapter-layer error (schema conversion, ID normalization, etc.).
    #[error("adapter error: {0}")]
    Adapter(#[from] AdapterError),

    /// Strict mode transformation failed.
    #[error("strict mode conversion failed: {0}")]
    StrictMode(String),
}

// ---- Convert options --------------------------------------------------------

/// Options for the `*_with_options` variants of [`OpenAIConverter`] methods.
///
/// Constructed via [`ConvertOptions::default`] or [`ConvertOptions::new`]
/// + the chainable setters. Each setter returns `Self` so callers can
///   build the options inline:
///
/// ```ignore
/// let tools = converter.convert_registry_json_with_options(
///     &registry_json,
///     ConvertOptions::default()
///         .with_embed_annotations(true)
///         .with_rich_description(true),
///     None,
///     None,
/// )?;
/// ```
///
/// The fieldful options struct exists so adding cross-cutting flags
/// (currently `rich_description`, future possibilities like
/// `embed_examples`, `compact_schema`) doesn't ratchet the positional
/// signature of every public method.
#[derive(Debug, Clone, Default)]
pub struct ConvertOptions {
    /// Append annotation hints (e.g. `[idempotent]`) to the description.
    pub embed_annotations: bool,
    /// Apply OpenAI strict-mode transformations to the schema.
    pub strict: bool,
    /// Replace the plain ``description`` with a Markdown body rendered
    /// by `apcore_toolkit::format_module(Markdown)` — title,
    /// description, parameters, returns, behavior table, tags,
    /// examples. LLMs select tools primarily from this string;
    /// Markdown packs more decision-relevant signal per token.
    pub rich_description: bool,
}

impl ConvertOptions {
    /// Construct with all flags off (same as `Default`).
    pub fn new() -> Self {
        Self::default()
    }

    /// Toggle annotation embedding.
    pub fn with_embed_annotations(mut self, embed: bool) -> Self {
        self.embed_annotations = embed;
        self
    }

    /// Toggle OpenAI strict mode.
    pub fn with_strict(mut self, strict: bool) -> Self {
        self.strict = strict;
        self
    }

    /// Toggle Markdown rendering of `description`.
    pub fn with_rich_description(mut self, rich: bool) -> Self {
        self.rich_description = rich;
        self
    }
}

// ---- OpenAIConverter --------------------------------------------------------

/// Converts apcore registries and module descriptors to OpenAI-compatible
/// tool/function definitions.
///
/// Composes the adapter components ([`SchemaConverter`], [`AnnotationMapper`],
/// [`ModuleIDNormalizer`]) and adds OpenAI-specific strict mode processing.
pub struct OpenAIConverter {
    _schema_converter: SchemaConverter,
    _annotation_mapper: AnnotationMapper,
    _id_normalizer: ModuleIDNormalizer,
}

impl OpenAIConverter {
    /// Create a new `OpenAIConverter` with default adapter instances.
    pub fn new() -> Self {
        Self {
            _schema_converter: SchemaConverter,
            _annotation_mapper: AnnotationMapper,
            _id_normalizer: ModuleIDNormalizer,
        }
    }

    /// Convert all modules in an apcore [`Registry`] to OpenAI tool definitions.
    ///
    /// Primary public API matching Python+TS duck-typed Registry object interface.
    /// Calls `registry.list()` and `registry.get_definition()` to enumerate
    /// modules, then delegates to [`Self::convert_descriptor`]. [D11-024]
    ///
    /// # Arguments
    /// * `registry` — a live apcore Registry.
    /// * `embed_annotations` — if true, append annotation hints to descriptions.
    /// * `strict` — if true, enable OpenAI strict mode on schemas.
    /// * `tags` — if provided, only include modules whose tags contain ALL specified tags.
    /// * `prefix` — if provided, only include modules whose ID starts with the prefix.
    pub fn convert_registry(
        &self,
        registry: &Registry,
        embed_annotations: bool,
        strict: bool,
        tags: Option<&[&str]>,
        prefix: Option<&str>,
    ) -> Result<Vec<Value>, ConverterError> {
        self.convert_registry_with_options(
            registry,
            ConvertOptions::default()
                .with_embed_annotations(embed_annotations)
                .with_strict(strict),
            tags,
            prefix,
        )
    }

    /// Like [`Self::convert_registry`] but takes a [`ConvertOptions`]
    /// struct, supporting `rich_description` for Markdown-rendered tool
    /// descriptions (apcore-toolkit `format_module(Markdown)`).
    pub fn convert_registry_with_options(
        &self,
        registry: &Registry,
        options: ConvertOptions,
        tags: Option<&[&str]>,
        prefix: Option<&str>,
    ) -> Result<Vec<Value>, ConverterError> {
        let module_ids = registry.list(tags, prefix, None);
        let mut tools = Vec::new();
        let mut seen_names: HashMap<String, String> = HashMap::new();
        // Sort for deterministic output
        let mut sorted_ids = module_ids;
        sorted_ids.sort();

        for module_id in &sorted_ids {
            let Ok(Some(descriptor)) = registry.get_definition(module_id) else {
                continue;
            };
            // Description sourcing: when `rich_description` is on we
            // delegate straight to the markdown helper which has direct
            // access to the real `ModuleDescriptor` (including
            // documentation, examples, display overlay) — strictly
            // richer than the JSON-projection path. Otherwise fall back
            // to the descriptor's plain `description` field.
            let description = if options.rich_description {
                // [A-D-MD-3] The helper reports a render failure as None; the
                // plain description is the caller's fallback.
                markdown::render_module_markdown(&descriptor, true)
                    .unwrap_or_else(|| descriptor.description.clone())
            } else {
                descriptor.description.clone()
            };
            // Build a JSON Value from the descriptor for convert_descriptor.
            // We pass `rich_description: false` here because the description
            // is already finalized above.
            let descriptor_json = serde_json::json!({
                "input_schema": descriptor.input_schema,
                "annotations": descriptor.annotations,
                "tags": descriptor.tags,
            });
            let inner_options = options.clone().with_rich_description(false);
            let tool = self.convert_descriptor_with_options(
                module_id,
                &descriptor_json,
                &description,
                inner_options,
            )?;
            let tool_name = tool
                .get("function")
                .and_then(|f| f.get("name"))
                .and_then(|n| n.as_str())
                .unwrap_or("")
                .to_string();
            if let Some(existing) = seen_names.get(&tool_name) {
                if existing != module_id {
                    return Err(ConverterError::StrictMode(format!(
                        "OpenAI function-name collision: module ids '{existing}' and \
                         '{module_id}' both normalize to '{tool_name}'."
                    )));
                }
            }
            seen_names.insert(tool_name, module_id.clone());
            tools.push(tool);
        }
        Ok(tools)
    }

    /// Convert all modules in a registry to OpenAI tool definitions.
    ///
    /// The registry is a JSON object where each key is a module ID and each value
    /// is an object with `description`, `input_schema`, `annotations`, and
    /// optionally `tags` (array of strings).
    ///
    /// # Arguments
    /// * `registry` - A JSON object mapping module IDs to their descriptors.
    /// * `embed_annotations` - If true, append annotation hints to descriptions.
    /// * `strict` - If true, enable OpenAI strict mode on schemas.
    /// * `tags` - If provided, only include modules whose tags contain ALL specified tags.
    /// * `prefix` - If provided, only include modules whose ID starts with the prefix.
    ///
    /// # Returns
    /// A vector of OpenAI-compatible tool objects.
    pub fn convert_registry_json(
        &self,
        registry: &Value,
        embed_annotations: bool,
        strict: bool,
        tags: Option<&[&str]>,
        prefix: Option<&str>,
    ) -> Result<Vec<Value>, ConverterError> {
        self.convert_registry_json_with_options(
            registry,
            ConvertOptions::default()
                .with_embed_annotations(embed_annotations)
                .with_strict(strict),
            tags,
            prefix,
        )
    }

    /// Like [`Self::convert_registry`] but takes a [`ConvertOptions`]
    /// struct, supporting `rich_description` for Markdown-rendered tool
    /// descriptions. The JSON entry for each module is adapted to a
    /// transient [`ScannedModule`] via [`json_entry_to_scanned_module`]
    /// before delegating to `apcore_toolkit::format_module(Markdown)`.
    pub fn convert_registry_json_with_options(
        &self,
        registry: &Value,
        options: ConvertOptions,
        tags: Option<&[&str]>,
        prefix: Option<&str>,
    ) -> Result<Vec<Value>, ConverterError> {
        let modules = match registry.as_object() {
            Some(m) => m,
            None => return Ok(Vec::new()),
        };

        let mut tools = Vec::new();
        // [OC-3] Track normalized names so we can detect collisions.
        // OpenAI function names must be unique post-normalization
        // (dot→hyphen). E.g. `a.b` and `a-b` both normalize to `a-b`;
        // without this guard we'd silently emit two tools with
        // identical function.name, producing undefined OpenAI behavior.
        let mut seen_names: HashMap<String, String> = HashMap::new();
        // Sort keys for deterministic output
        let mut module_ids: Vec<&String> = modules.keys().collect();
        module_ids.sort();

        for module_id in module_ids {
            // Apply prefix filter
            if let Some(pfx) = prefix {
                if !module_id.starts_with(pfx) {
                    continue;
                }
            }

            let entry = match modules.get(module_id.as_str()) {
                Some(v) => v,
                None => continue,
            };

            // Apply tags filter
            if let Some(required_tags) = tags {
                let module_tags: Vec<&str> = entry
                    .get("tags")
                    .and_then(|t| t.as_array())
                    .map(|arr| arr.iter().filter_map(|v| v.as_str()).collect())
                    .unwrap_or_default();

                if !required_tags.iter().all(|t| module_tags.contains(t)) {
                    continue;
                }
            }

            let plain_description = entry
                .get("description")
                .and_then(|v| v.as_str())
                .unwrap_or("");

            // When rich_description is on, project the JSON entry into a
            // transient ScannedModule and delegate to apcore-toolkit's
            // `format_module(Markdown)`. This is the JSON-path equivalent
            // of what `convert_registry_with_options` does with
            // a real `ModuleDescriptor`. The toolkit's Markdown style
            // gracefully handles missing fields (empty `## Examples`,
            // empty `## Behavior` table) so a sparse JSON entry still
            // produces a usable rendering.
            let description: String = if options.rich_description {
                let scanned = json_entry_to_scanned_module(module_id, entry);
                match apcore_toolkit::format_module(
                    &scanned,
                    apcore_toolkit::ModuleStyle::Markdown,
                    true,
                ) {
                    apcore_toolkit::FormatOutput::Text(text) => text,
                    _ => plain_description.to_string(),
                }
            } else {
                plain_description.to_string()
            };

            // Pass rich_description=false to the inner call — the
            // description is already finalized above.
            let inner_options = options.clone().with_rich_description(false);
            let tool = self.convert_descriptor_with_options(
                module_id,
                entry,
                &description,
                inner_options,
            )?;
            // Extract the function name from the tool envelope.
            let tool_name = tool
                .get("function")
                .and_then(|f| f.get("name"))
                .and_then(|n| n.as_str())
                .unwrap_or("")
                .to_string();
            if let Some(existing) = seen_names.get(&tool_name) {
                if existing != module_id {
                    return Err(ConverterError::StrictMode(format!(
                        "OpenAI function-name collision: module ids '{existing}' and \
                         '{module_id}' both normalize to '{tool_name}'. OpenAI requires \
                         unique function names; rename one of the modules to avoid the collision."
                    )));
                }
            }
            seen_names.insert(tool_name, module_id.clone());
            tools.push(tool);
        }

        Ok(tools)
    }

    /// Convert a single apcore module descriptor to an OpenAI tool definition.
    ///
    /// # Arguments
    /// * `name` - The module ID (dot-separated).
    /// * `descriptor` - The module descriptor as a JSON value containing
    ///   `input_schema` and optionally `annotations`.
    /// * `description` - The module description string.
    /// * `embed_annotations` - If true, append annotation hints to description.
    /// * `strict` - If true, enable OpenAI strict mode.
    ///
    /// # Returns
    /// An OpenAI-compatible tool object with `type`, `function.name`,
    /// `function.description`, `function.parameters`, and optionally `function.strict`.
    pub fn convert_descriptor(
        &self,
        name: &str,
        descriptor: &Value,
        description: &str,
        embed_annotations: bool,
        strict: bool,
    ) -> Result<Value, ConverterError> {
        self.convert_descriptor_with_options(
            name,
            descriptor,
            description,
            ConvertOptions::default()
                .with_embed_annotations(embed_annotations)
                .with_strict(strict),
        )
    }

    /// Like [`Self::convert_descriptor`] but takes a [`ConvertOptions`]
    /// struct. When `options.rich_description` is set the supplied
    /// `description` is replaced by `apcore_toolkit::format_module`
    /// Markdown rendering of a transient ScannedModule projected from
    /// the JSON `descriptor`.
    pub fn convert_descriptor_with_options(
        &self,
        name: &str,
        descriptor: &Value,
        description: &str,
        options: ConvertOptions,
    ) -> Result<Value, ConverterError> {
        // Normalize the module ID (dot -> dash)
        let normalized_name = ModuleIDNormalizer::normalize(name)?;

        // Convert input schema
        let input_schema = descriptor
            .get("input_schema")
            .cloned()
            .unwrap_or(Value::Null);
        // OpenAI converter keeps legacy (non-strict) behavior; callers can
        // opt-in separately. MCP tool schemas in factory.rs use strict mode.
        let mut parameters = SchemaConverter::convert_input_schema_strict(&input_schema, false)?;

        // Resolve the LLM-facing description. `rich_description` swaps
        // the supplied plain description for a Markdown body rendered
        // via `apcore_toolkit::format_module`. The annotation suffix is
        // appended afterwards as a strict superset.
        let mut desc = if options.rich_description {
            let scanned = json_entry_to_scanned_module(name, descriptor);
            match apcore_toolkit::format_module(
                &scanned,
                apcore_toolkit::ModuleStyle::Markdown,
                true,
            ) {
                apcore_toolkit::FormatOutput::Text(text) => text,
                _ => description.to_string(),
            }
        } else {
            description.to_string()
        };
        if options.embed_annotations {
            // Deserialize annotations from the descriptor JSON into ModuleAnnotations
            let annotations: Option<ModuleAnnotations> = descriptor
                .get("annotations")
                .and_then(|v| serde_json::from_value(v.clone()).ok());
            let suffix = AnnotationMapper::to_description_suffix(annotations.as_ref());
            desc.push_str(&suffix);
        }

        // Apply strict mode if requested
        if options.strict {
            parameters = Self::apply_strict_mode(&parameters);
        }

        // Build the function object
        let mut function = serde_json::json!({
            "name": normalized_name,
            "description": desc,
            "parameters": parameters,
        });

        if options.strict {
            function["strict"] = serde_json::json!(true);
        }

        Ok(serde_json::json!({
            "type": "function",
            "function": function,
        }))
    }

    // ---- Strict mode helpers (private) --------------------------------------

    /// Apply OpenAI strict mode transformations to a schema.
    ///
    /// 1. Promotes `x-llm-description` to `description` where both exist.
    /// 2. Strips all `x-*` extension keys and `default` keys.
    /// 3. Enforces strict mode rules (`additionalProperties: false`, all
    ///    properties required, optional properties become nullable).
    fn apply_strict_mode(schema: &Value) -> Value {
        let mut schema = schema.clone();
        Self::apply_llm_descriptions(&mut schema);
        Self::strip_extensions(&mut schema);
        Self::convert_to_strict(&mut schema);
        schema
    }

    /// Replace `description` with `x-llm-description` where both exist.
    /// Recurses into properties, items, oneOf/anyOf/allOf, $defs/definitions.
    fn apply_llm_descriptions(node: &mut Value) {
        let obj = match node.as_object_mut() {
            Some(o) => o,
            None => return,
        };

        // Promote x-llm-description to description
        if let Some(llm_desc) = obj.get("x-llm-description").cloned() {
            if obj.contains_key("description") {
                obj.insert("description".to_string(), llm_desc);
            }
        }

        // Recurse into properties
        if let Some(Value::Object(props)) = obj.get_mut("properties") {
            for prop in props.values_mut() {
                Self::apply_llm_descriptions(prop);
            }
        }

        // Recurse into items
        if let Some(items) = obj.get_mut("items") {
            if items.is_object() {
                Self::apply_llm_descriptions(items);
            }
        }

        // Recurse into oneOf/anyOf/allOf
        for keyword in &["oneOf", "anyOf", "allOf"] {
            if let Some(Value::Array(arr)) = obj.get_mut(*keyword) {
                for sub in arr.iter_mut() {
                    Self::apply_llm_descriptions(sub);
                }
            }
        }

        // Recurse into $defs/definitions
        for defs_key in &["$defs", "definitions"] {
            if let Some(Value::Object(defs)) = obj.get_mut(*defs_key) {
                for defn in defs.values_mut() {
                    Self::apply_llm_descriptions(defn);
                }
            }
        }
    }

    /// Remove all `x-*` keys and `default` keys recursively.
    fn strip_extensions(node: &mut Value) {
        match node {
            Value::Object(map) => {
                // Remove x-* and default keys
                map.retain(|k, _| !k.starts_with("x-") && k != "default");
                // Recurse into all remaining values
                for value in map.values_mut() {
                    Self::strip_extensions(value);
                }
            }
            Value::Array(arr) => {
                for item in arr.iter_mut() {
                    Self::strip_extensions(item);
                }
            }
            _ => {}
        }
    }

    /// Enforce strict mode rules on an object schema. Mutates in place.
    ///
    /// For objects with `properties`:
    /// - Sets `additionalProperties: false`
    /// - Identifies optional properties (not already in `required`)
    /// - Makes optional properties nullable (type array or oneOf wrapping)
    /// - Sets `required` to sorted list of all property names
    ///
    /// Recurses into properties, items, oneOf/anyOf/allOf, $defs/definitions.
    fn convert_to_strict(node: &mut Value) {
        let obj = match node.as_object_mut() {
            Some(o) => o,
            None => return,
        };

        // Process object schemas with properties
        if obj.get("type") == Some(&Value::String("object".to_string()))
            && obj.contains_key("properties")
        {
            // Set additionalProperties: false
            obj.insert("additionalProperties".to_string(), Value::Bool(false));

            // Collect existing required set
            let existing_required: std::collections::HashSet<String> = obj
                .get("required")
                .and_then(|v| v.as_array())
                .map(|arr| {
                    arr.iter()
                        .filter_map(|v| v.as_str().map(String::from))
                        .collect()
                })
                .unwrap_or_default();

            // Get all property names
            let all_names: Vec<String> = obj
                .get("properties")
                .and_then(|v| v.as_object())
                .map(|props| props.keys().cloned().collect())
                .unwrap_or_default();

            // Identify optional properties
            let optional_names: Vec<String> = all_names
                .iter()
                .filter(|n| !existing_required.contains(n.as_str()))
                .cloned()
                .collect();

            // Make optional properties nullable
            if let Some(Value::Object(props)) = obj.get_mut("properties") {
                for name in &optional_names {
                    if let Some(prop) = props.get_mut(name) {
                        Self::make_nullable(prop);
                    }
                }
            }

            // Set required to sorted list of all property names
            let mut sorted_names = all_names;
            sorted_names.sort();
            obj.insert(
                "required".to_string(),
                Value::Array(sorted_names.into_iter().map(Value::String).collect()),
            );
        }

        // Recurse into properties
        if let Some(Value::Object(props)) = obj.get_mut("properties") {
            for prop in props.values_mut() {
                Self::convert_to_strict(prop);
            }
        }

        // Recurse into items
        if let Some(items) = obj.get_mut("items") {
            if items.is_object() {
                Self::convert_to_strict(items);
            }
        }

        // Recurse into oneOf/anyOf/allOf
        for keyword in &["oneOf", "anyOf", "allOf"] {
            if let Some(Value::Array(arr)) = obj.get_mut(*keyword) {
                for sub in arr.iter_mut() {
                    Self::convert_to_strict(sub);
                }
            }
        }

        // Recurse into $defs/definitions
        for defs_key in &["$defs", "definitions"] {
            if let Some(Value::Object(defs)) = obj.get_mut(*defs_key) {
                for defn in defs.values_mut() {
                    Self::convert_to_strict(defn);
                }
            }
        }
    }

    /// Make a property nullable by modifying its type or wrapping in oneOf.
    fn make_nullable(prop: &mut Value) {
        if let Some(obj) = prop.as_object_mut() {
            if let Some(type_val) = obj.get_mut("type") {
                match type_val {
                    Value::String(s) => {
                        // "string" -> ["string", "null"]
                        let original = s.clone();
                        *type_val = Value::Array(vec![
                            Value::String(original),
                            Value::String("null".to_string()),
                        ]);
                    }
                    Value::Array(arr) if !arr.contains(&Value::String("null".to_string())) => {
                        // ["string", "integer"] -> ["string", "integer", "null"]
                        arr.push(Value::String("null".to_string()));
                    }
                    _ => {}
                }
            } else {
                // No type key (pure $ref or composition) — wrap in oneOf with null
                let original = prop.clone();
                *prop = serde_json::json!({
                    "oneOf": [original, {"type": "null"}]
                });
            }
        }
    }
}

impl Default for OpenAIConverter {
    fn default() -> Self {
        Self::new()
    }
}

// ---- JSON → ScannedModule adapter -------------------------------------------

/// Project a JSON entry from the duck-typed registry-as-JSON form
/// (`{module_id: {description, input_schema, output_schema?, annotations,
/// tags, examples?, metadata?, ...}}`) into an `apcore_toolkit::ScannedModule`
/// suitable for `format_module` Markdown rendering.
///
/// This is the adapter that lets [`OpenAIConverter::convert_registry`] —
/// the JSON path — opt in to `rich_description=true` even though it
/// doesn't have a real [`apcore::registry::ModuleDescriptor`] in hand.
/// Missing fields fall back to sensible defaults so the toolkit's
/// markdown sections (`## Parameters`, `## Returns`, `## Behavior`,
/// `## Tags`, `## Examples`) gracefully render whatever the entry
/// actually carries.
pub(crate) fn json_entry_to_scanned_module(module_id: &str, entry: &Value) -> ScannedModule {
    let description = entry
        .get("description")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();
    let input_schema = entry
        .get("input_schema")
        .cloned()
        .unwrap_or_else(|| serde_json::json!({}));
    let output_schema = entry
        .get("output_schema")
        .cloned()
        .unwrap_or_else(|| serde_json::json!({}));
    let tags: Vec<String> = entry
        .get("tags")
        .and_then(|v| v.as_array())
        .map(|arr| {
            arr.iter()
                .filter_map(|t| t.as_str().map(|s| s.to_string()))
                .collect()
        })
        .unwrap_or_default();
    let version = entry
        .get("version")
        .and_then(|v| v.as_str())
        .unwrap_or("1.0.0")
        .to_string();
    let annotations: Option<ModuleAnnotations> = entry
        .get("annotations")
        .and_then(|v| serde_json::from_value(v.clone()).ok());
    let documentation = entry
        .get("documentation")
        .and_then(|v| v.as_str())
        .map(|s| s.to_string());
    let examples: Vec<ModuleExample> = entry
        .get("examples")
        .and_then(|v| v.as_array())
        .map(|arr| {
            arr.iter()
                .filter_map(|e| serde_json::from_value(e.clone()).ok())
                .collect()
        })
        .unwrap_or_default();
    let metadata: HashMap<String, Value> = entry
        .get("metadata")
        .and_then(|v| v.as_object())
        .map(|obj| obj.iter().map(|(k, v)| (k.clone(), v.clone())).collect())
        .unwrap_or_default();
    let display = entry.get("display").cloned();
    ScannedModule {
        module_id: module_id.to_string(),
        description,
        input_schema,
        output_schema,
        tags,
        target: String::new(),
        version,
        annotations,
        documentation,
        suggested_alias: None,
        examples,
        metadata,
        display,
        warnings: vec![],
    }
}

// ---- Tests ------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    // ---- Task 1: converter-types tests --------------------------------------

    #[test]
    fn test_converter_error_display_adapter() {
        let adapter_err = AdapterError::SchemaConversion("bad schema".into());
        let err = ConverterError::Adapter(adapter_err);
        assert_eq!(
            err.to_string(),
            "adapter error: schema conversion failed: bad schema"
        );
    }

    #[test]
    fn test_converter_error_display_strict() {
        let err = ConverterError::StrictMode("cannot make nullable".into());
        assert_eq!(
            err.to_string(),
            "strict mode conversion failed: cannot make nullable"
        );
    }

    #[test]
    fn test_converter_error_from_adapter() {
        let adapter_err = AdapterError::SchemaConversion("test".into());
        let err: ConverterError = adapter_err.into();
        assert!(matches!(err, ConverterError::Adapter(_)));
    }

    #[test]
    fn test_openai_converter_new() {
        let _converter = OpenAIConverter::new();
        // Constructs without panic
    }

    #[test]
    fn test_openai_converter_default() {
        let _converter = OpenAIConverter::default();
        // Default impl works without panic
    }

    // ---- Task 2: strict-mode tests ------------------------------------------

    // ---- apply_llm_descriptions tests ----

    #[test]
    fn test_apply_llm_descriptions_replaces_description() {
        let mut node = json!({
            "description": "Original",
            "x-llm-description": "LLM version"
        });
        OpenAIConverter::apply_llm_descriptions(&mut node);
        assert_eq!(node["description"], "LLM version");
    }

    #[test]
    fn test_apply_llm_descriptions_preserves_when_no_llm() {
        let mut node = json!({
            "description": "Original"
        });
        OpenAIConverter::apply_llm_descriptions(&mut node);
        assert_eq!(node["description"], "Original");
    }

    #[test]
    fn test_apply_llm_descriptions_nested_properties() {
        let mut node = json!({
            "type": "object",
            "properties": {
                "name": {
                    "description": "Old",
                    "x-llm-description": "New"
                }
            }
        });
        OpenAIConverter::apply_llm_descriptions(&mut node);
        assert_eq!(node["properties"]["name"]["description"], "New");
    }

    #[test]
    fn test_apply_llm_descriptions_nested_items() {
        let mut node = json!({
            "type": "array",
            "items": {
                "description": "Old",
                "x-llm-description": "New"
            }
        });
        OpenAIConverter::apply_llm_descriptions(&mut node);
        assert_eq!(node["items"]["description"], "New");
    }

    #[test]
    fn test_apply_llm_descriptions_nested_oneof() {
        let mut node = json!({
            "oneOf": [
                {"description": "A", "x-llm-description": "A-LLM"},
                {"description": "B"}
            ]
        });
        OpenAIConverter::apply_llm_descriptions(&mut node);
        assert_eq!(node["oneOf"][0]["description"], "A-LLM");
        assert_eq!(node["oneOf"][1]["description"], "B");
    }

    #[test]
    fn test_apply_llm_descriptions_nested_defs() {
        let mut node = json!({
            "$defs": {
                "Foo": {
                    "description": "Old",
                    "x-llm-description": "New"
                }
            },
            "definitions": {
                "Bar": {
                    "description": "Old",
                    "x-llm-description": "New"
                }
            }
        });
        OpenAIConverter::apply_llm_descriptions(&mut node);
        assert_eq!(node["$defs"]["Foo"]["description"], "New");
        assert_eq!(node["definitions"]["Bar"]["description"], "New");
    }

    // ---- strip_extensions tests ----

    #[test]
    fn test_strip_extensions_removes_x_keys() {
        let mut node = json!({
            "type": "string",
            "x-custom": "value",
            "x-another": 42
        });
        OpenAIConverter::strip_extensions(&mut node);
        assert!(node.get("x-custom").is_none());
        assert!(node.get("x-another").is_none());
        assert_eq!(node["type"], "string");
    }

    #[test]
    fn test_strip_extensions_removes_defaults() {
        let mut node = json!({
            "type": "string",
            "default": "hello"
        });
        OpenAIConverter::strip_extensions(&mut node);
        assert!(node.get("default").is_none());
        assert_eq!(node["type"], "string");
    }

    #[test]
    fn test_strip_extensions_recursive() {
        let mut node = json!({
            "type": "object",
            "properties": {
                "name": {
                    "type": "string",
                    "x-custom": "remove",
                    "default": "val"
                }
            },
            "items": {
                "x-ext": true
            }
        });
        OpenAIConverter::strip_extensions(&mut node);
        assert!(node["properties"]["name"].get("x-custom").is_none());
        assert!(node["properties"]["name"].get("default").is_none());
        assert!(node["items"].get("x-ext").is_none());
    }

    #[test]
    fn test_strip_extensions_preserves_non_x_keys() {
        let mut node = json!({
            "type": "string",
            "description": "keep",
            "enum": ["a", "b"],
            "x-remove": true
        });
        OpenAIConverter::strip_extensions(&mut node);
        assert_eq!(node["type"], "string");
        assert_eq!(node["description"], "keep");
        assert_eq!(node["enum"], json!(["a", "b"]));
        assert!(node.get("x-remove").is_none());
    }

    // ---- convert_to_strict tests ----

    #[test]
    fn test_convert_to_strict_sets_additional_properties_false() {
        let mut node = json!({
            "type": "object",
            "properties": {
                "name": {"type": "string"}
            }
        });
        OpenAIConverter::convert_to_strict(&mut node);
        assert_eq!(node["additionalProperties"], false);
    }

    #[test]
    fn test_convert_to_strict_makes_all_required() {
        let mut node = json!({
            "type": "object",
            "properties": {
                "zebra": {"type": "string"},
                "alpha": {"type": "integer"}
            }
        });
        OpenAIConverter::convert_to_strict(&mut node);
        assert_eq!(node["required"], json!(["alpha", "zebra"]));
    }

    #[test]
    fn test_convert_to_strict_nullable_optional_string() {
        let mut node = json!({
            "type": "object",
            "properties": {
                "name": {"type": "string"}
            }
        });
        OpenAIConverter::convert_to_strict(&mut node);
        assert_eq!(
            node["properties"]["name"]["type"],
            json!(["string", "null"])
        );
    }

    #[test]
    fn test_convert_to_strict_nullable_optional_array_type() {
        let mut node = json!({
            "type": "object",
            "properties": {
                "value": {"type": ["string", "integer"]}
            }
        });
        OpenAIConverter::convert_to_strict(&mut node);
        assert_eq!(
            node["properties"]["value"]["type"],
            json!(["string", "integer", "null"])
        );
    }

    #[test]
    fn test_convert_to_strict_nullable_optional_ref() {
        let mut node = json!({
            "type": "object",
            "properties": {
                "config": {"$ref": "#/$defs/Config"}
            }
        });
        OpenAIConverter::convert_to_strict(&mut node);
        let config = &node["properties"]["config"];
        assert!(config.get("oneOf").is_some());
        let one_of = config["oneOf"].as_array().unwrap();
        assert_eq!(one_of.len(), 2);
        assert_eq!(one_of[0], json!({"$ref": "#/$defs/Config"}));
        assert_eq!(one_of[1], json!({"type": "null"}));
    }

    #[test]
    fn test_convert_to_strict_preserves_required() {
        let mut node = json!({
            "type": "object",
            "properties": {
                "required_field": {"type": "string"},
                "optional_field": {"type": "integer"}
            },
            "required": ["required_field"]
        });
        OpenAIConverter::convert_to_strict(&mut node);
        // required_field should NOT become nullable (it was already required)
        assert_eq!(node["properties"]["required_field"]["type"], "string");
        // optional_field SHOULD become nullable
        assert_eq!(
            node["properties"]["optional_field"]["type"],
            json!(["integer", "null"])
        );
        // Both should be in required now
        assert_eq!(
            node["required"],
            json!(["optional_field", "required_field"])
        );
    }

    #[test]
    fn test_convert_to_strict_recursive_nested_object() {
        // When inner is already required, its type stays "object" and recursion applies
        let mut node = json!({
            "type": "object",
            "properties": {
                "inner": {
                    "type": "object",
                    "properties": {
                        "value": {"type": "string"}
                    }
                }
            },
            "required": ["inner"]
        });
        OpenAIConverter::convert_to_strict(&mut node);
        // inner was required so type stays "object", recursion applies strict rules
        assert_eq!(node["properties"]["inner"]["additionalProperties"], false);
        assert_eq!(
            node["properties"]["inner"]["properties"]["value"]["type"],
            json!(["string", "null"])
        );
        assert_eq!(node["properties"]["inner"]["required"], json!(["value"]));
    }

    #[test]
    fn test_convert_to_strict_recursive_items() {
        let mut node = json!({
            "type": "array",
            "items": {
                "type": "object",
                "properties": {
                    "id": {"type": "integer"}
                }
            }
        });
        OpenAIConverter::convert_to_strict(&mut node);
        assert_eq!(node["items"]["additionalProperties"], false);
        assert_eq!(node["items"]["required"], json!(["id"]));
    }

    #[test]
    fn test_convert_to_strict_recursive_oneof() {
        let mut node = json!({
            "oneOf": [
                {
                    "type": "object",
                    "properties": {
                        "a": {"type": "string"}
                    }
                }
            ]
        });
        OpenAIConverter::convert_to_strict(&mut node);
        assert_eq!(node["oneOf"][0]["additionalProperties"], false);
        assert_eq!(node["oneOf"][0]["required"], json!(["a"]));
    }

    // ---- apply_strict_mode end-to-end test ----

    #[test]
    fn test_apply_strict_mode_full_pipeline() {
        let schema = json!({
            "type": "object",
            "properties": {
                "query": {
                    "type": "string",
                    "description": "Search query",
                    "x-llm-description": "The search term to look for"
                },
                "limit": {
                    "type": "integer",
                    "default": 10,
                    "x-validation": "positive"
                },
                "config": {
                    "type": "object",
                    "properties": {
                        "verbose": {
                            "type": "boolean",
                            "default": false
                        }
                    }
                }
            },
            "required": ["query"]
        });

        let result = OpenAIConverter::apply_strict_mode(&schema);

        // 1. x-llm-description promoted to description
        assert_eq!(
            result["properties"]["query"]["description"],
            "The search term to look for"
        );

        // 2. x-* and default stripped
        assert!(result["properties"]["query"]
            .get("x-llm-description")
            .is_none());
        assert!(result["properties"]["limit"].get("default").is_none());
        assert!(result["properties"]["limit"].get("x-validation").is_none());
        assert!(result["properties"]["config"]["properties"]["verbose"]
            .get("default")
            .is_none());

        // 3. additionalProperties: false on root object
        assert_eq!(result["additionalProperties"], false);
        // Note: config is optional so its type becomes ["object", "null"],
        // which means the strict recursion won't apply additionalProperties to it.
        // This matches the Python behavior.

        // 4. All properties required (sorted)
        assert_eq!(result["required"], json!(["config", "limit", "query"]));

        // 5. Optional properties nullable
        // query was already required -> NOT nullable
        assert_eq!(result["properties"]["query"]["type"], "string");
        // limit was optional -> nullable
        assert_eq!(
            result["properties"]["limit"]["type"],
            json!(["integer", "null"])
        );
        // config was optional -> nullable (type becomes array)
        assert_eq!(
            result["properties"]["config"]["type"],
            json!(["object", "null"])
        );
    }

    // ---- Edge cases ----

    #[test]
    fn test_strip_extensions_array_with_objects() {
        let mut node = json!({
            "oneOf": [
                {"type": "string", "x-ext": true},
                {"type": "integer", "default": 0}
            ]
        });
        OpenAIConverter::strip_extensions(&mut node);
        assert!(node["oneOf"][0].get("x-ext").is_none());
        assert!(node["oneOf"][1].get("default").is_none());
    }

    #[test]
    fn test_convert_to_strict_no_properties_no_change() {
        let mut node = json!({"type": "string"});
        let original = node.clone();
        OpenAIConverter::convert_to_strict(&mut node);
        assert_eq!(node, original);
    }

    #[test]
    fn test_apply_llm_descriptions_no_description_key() {
        // x-llm-description without description should NOT create description
        let mut node = json!({
            "x-llm-description": "LLM only"
        });
        OpenAIConverter::apply_llm_descriptions(&mut node);
        assert!(node.get("description").is_none());
        assert_eq!(node["x-llm-description"], "LLM only");
    }

    #[test]
    fn test_convert_to_strict_already_nullable_not_doubled() {
        let mut node = json!({
            "type": "object",
            "properties": {
                "value": {"type": ["string", "null"]}
            }
        });
        OpenAIConverter::convert_to_strict(&mut node);
        // Should not add another "null"
        assert_eq!(
            node["properties"]["value"]["type"],
            json!(["string", "null"])
        );
    }

    // ---- Task 3: convert-descriptor tests -----------------------------------

    #[test]
    fn test_convert_descriptor_basic() {
        let converter = OpenAIConverter::new();
        let descriptor = json!({
            "input_schema": {
                "type": "object",
                "properties": {
                    "query": {"type": "string"}
                }
            }
        });
        let result = converter
            .convert_descriptor("image.resize", &descriptor, "Resize an image", false, false)
            .unwrap();
        assert_eq!(result["type"], "function");
        assert_eq!(result["function"]["name"], "image-resize");
        assert_eq!(result["function"]["description"], "Resize an image");
        assert_eq!(result["function"]["parameters"]["type"], "object");
        assert!(result["function"].get("strict").is_none());
    }

    #[test]
    fn test_convert_descriptor_name_normalized() {
        let converter = OpenAIConverter::new();
        let descriptor = json!({"input_schema": {}});
        let result = converter
            .convert_descriptor("image.resize", &descriptor, "desc", false, false)
            .unwrap();
        assert_eq!(result["function"]["name"], "image-resize");
    }

    #[test]
    fn test_convert_descriptor_schema_converted() {
        let converter = OpenAIConverter::new();
        // Empty schema should become {type: "object", properties: {}}
        let descriptor = json!({"input_schema": {}});
        let result = converter
            .convert_descriptor("ping", &descriptor, "Ping", false, false)
            .unwrap();
        assert_eq!(
            result["function"]["parameters"],
            json!({"type": "object", "properties": {}})
        );
    }

    #[test]
    fn test_convert_descriptor_with_annotations() {
        let converter = OpenAIConverter::new();
        let descriptor = json!({
            "input_schema": {},
            "annotations": {
                "destructive": true
            }
        });
        let result = converter
            .convert_descriptor("tool.delete", &descriptor, "Delete something", true, false)
            .unwrap();
        let desc = result["function"]["description"].as_str().unwrap();
        assert!(desc.starts_with("Delete something"));
        assert!(desc.contains("WARNING"));
        assert!(desc.contains("DESTRUCTIVE"));
    }

    #[test]
    fn test_convert_descriptor_without_annotations() {
        let converter = OpenAIConverter::new();
        let descriptor = json!({
            "input_schema": {},
            "annotations": {
                "destructive": true
            }
        });
        let result = converter
            .convert_descriptor("tool.delete", &descriptor, "Delete something", false, false)
            .unwrap();
        assert_eq!(result["function"]["description"], "Delete something");
    }

    #[test]
    fn test_convert_descriptor_with_strict() {
        let converter = OpenAIConverter::new();
        let descriptor = json!({
            "input_schema": {
                "type": "object",
                "properties": {
                    "name": {"type": "string"}
                }
            }
        });
        let result = converter
            .convert_descriptor("ping", &descriptor, "Ping", false, true)
            .unwrap();
        assert_eq!(result["function"]["strict"], true);
        // Strict mode should add additionalProperties: false
        assert_eq!(
            result["function"]["parameters"]["additionalProperties"],
            false
        );
    }

    #[test]
    fn test_convert_descriptor_without_strict() {
        let converter = OpenAIConverter::new();
        let descriptor = json!({"input_schema": {}});
        let result = converter
            .convert_descriptor("ping", &descriptor, "Ping", false, false)
            .unwrap();
        assert!(result["function"].get("strict").is_none());
    }

    #[test]
    fn test_convert_descriptor_strict_transforms_schema() {
        let converter = OpenAIConverter::new();
        let descriptor = json!({
            "input_schema": {
                "type": "object",
                "properties": {
                    "query": {"type": "string"},
                    "limit": {"type": "integer", "default": 10}
                },
                "required": ["query"]
            }
        });
        let result = converter
            .convert_descriptor("search", &descriptor, "Search", false, true)
            .unwrap();
        let params = &result["function"]["parameters"];
        assert_eq!(params["additionalProperties"], false);
        // All properties required (sorted)
        assert_eq!(params["required"], json!(["limit", "query"]));
        // limit was optional -> nullable
        assert_eq!(
            params["properties"]["limit"]["type"],
            json!(["integer", "null"])
        );
        // default should be stripped
        assert!(params["properties"]["limit"].get("default").is_none());
        // query was required -> stays as-is
        assert_eq!(params["properties"]["query"]["type"], "string");
    }

    #[test]
    fn test_convert_descriptor_null_annotations_with_embed() {
        let converter = OpenAIConverter::new();
        // No annotations field at all
        let descriptor = json!({"input_schema": {}});
        let result = converter
            .convert_descriptor("ping", &descriptor, "Ping", true, false)
            .unwrap();
        // Description should be unchanged (no suffix for None annotations)
        assert_eq!(result["function"]["description"], "Ping");
    }

    #[test]
    fn test_convert_descriptor_default_annotations_with_embed() {
        let converter = OpenAIConverter::new();
        // Default annotations (all defaults) — should produce no suffix
        let descriptor = json!({
            "input_schema": {},
            "annotations": {}
        });
        let result = converter
            .convert_descriptor("ping", &descriptor, "Ping", true, false)
            .unwrap();
        assert_eq!(result["function"]["description"], "Ping");
    }

    #[test]
    fn test_convert_descriptor_destructive_annotation_warning() {
        let converter = OpenAIConverter::new();
        let descriptor = json!({
            "input_schema": {},
            "annotations": {
                "destructive": true
            }
        });
        let result = converter
            .convert_descriptor("tool.nuke", &descriptor, "Nuke everything", true, false)
            .unwrap();
        let desc = result["function"]["description"].as_str().unwrap();
        assert!(desc.contains("WARNING"));
        assert!(desc.contains("DESTRUCTIVE"));
    }

    // ---- Task 4: convert-registry tests -------------------------------------

    #[test]
    fn test_convert_registry_empty() {
        let converter = OpenAIConverter::new();
        let registry = json!({});
        let result = converter
            .convert_registry_json(&registry, false, false, None, None)
            .unwrap();
        assert!(result.is_empty());
    }

    #[test]
    fn test_convert_registry_null() {
        let converter = OpenAIConverter::new();
        let registry = Value::Null;
        let result = converter
            .convert_registry_json(&registry, false, false, None, None)
            .unwrap();
        assert!(result.is_empty());
    }

    #[test]
    fn test_convert_registry_single_module() {
        let converter = OpenAIConverter::new();
        let registry = json!({
            "image.resize": {
                "description": "Resize an image",
                "input_schema": {
                    "type": "object",
                    "properties": {
                        "width": {"type": "integer"}
                    }
                }
            }
        });
        let result = converter
            .convert_registry_json(&registry, false, false, None, None)
            .unwrap();
        assert_eq!(result.len(), 1);
        assert_eq!(result[0]["type"], "function");
        assert_eq!(result[0]["function"]["name"], "image-resize");
        assert_eq!(result[0]["function"]["description"], "Resize an image");
    }

    #[test]
    fn test_convert_registry_multiple_modules() {
        let converter = OpenAIConverter::new();
        let registry = json!({
            "image.resize": {
                "description": "Resize",
                "input_schema": {}
            },
            "image.crop": {
                "description": "Crop",
                "input_schema": {}
            },
            "text.summarize": {
                "description": "Summarize",
                "input_schema": {}
            }
        });
        let result = converter
            .convert_registry_json(&registry, false, false, None, None)
            .unwrap();
        assert_eq!(result.len(), 3);
    }

    #[test]
    fn test_convert_registry_with_tags_filter() {
        let converter = OpenAIConverter::new();
        let registry = json!({
            "image.resize": {
                "description": "Resize",
                "input_schema": {},
                "tags": ["image", "transform"]
            },
            "image.crop": {
                "description": "Crop",
                "input_schema": {},
                "tags": ["image"]
            },
            "text.summarize": {
                "description": "Summarize",
                "input_schema": {},
                "tags": ["text"]
            }
        });
        // Filter by "image" tag — should include resize and crop
        let result = converter
            .convert_registry_json(&registry, false, false, Some(&["image"]), None)
            .unwrap();
        assert_eq!(result.len(), 2);

        // Filter by both "image" and "transform" — should only include resize
        let result = converter
            .convert_registry_json(&registry, false, false, Some(&["image", "transform"]), None)
            .unwrap();
        assert_eq!(result.len(), 1);
        assert_eq!(result[0]["function"]["name"], "image-resize");
    }

    #[test]
    fn test_convert_registry_with_prefix_filter() {
        let converter = OpenAIConverter::new();
        let registry = json!({
            "image.resize": {
                "description": "Resize",
                "input_schema": {}
            },
            "image.crop": {
                "description": "Crop",
                "input_schema": {}
            },
            "text.summarize": {
                "description": "Summarize",
                "input_schema": {}
            }
        });
        let result = converter
            .convert_registry_json(&registry, false, false, None, Some("image"))
            .unwrap();
        assert_eq!(result.len(), 2);
        // All names should start with "image-"
        for tool in &result {
            assert!(tool["function"]["name"]
                .as_str()
                .unwrap()
                .starts_with("image-"));
        }
    }

    #[test]
    fn test_convert_registry_passes_embed_annotations() {
        let converter = OpenAIConverter::new();
        let registry = json!({
            "tool.delete": {
                "description": "Delete data",
                "input_schema": {},
                "annotations": {
                    "destructive": true
                }
            }
        });
        let result = converter
            .convert_registry_json(&registry, true, false, None, None)
            .unwrap();
        assert_eq!(result.len(), 1);
        let desc = result[0]["function"]["description"].as_str().unwrap();
        assert!(desc.contains("DESTRUCTIVE"));
    }

    #[test]
    fn test_convert_registry_passes_strict() {
        let converter = OpenAIConverter::new();
        let registry = json!({
            "ping": {
                "description": "Ping",
                "input_schema": {
                    "type": "object",
                    "properties": {
                        "target": {"type": "string"}
                    }
                }
            }
        });
        let result = converter
            .convert_registry_json(&registry, false, true, None, None)
            .unwrap();
        assert_eq!(result.len(), 1);
        assert_eq!(result[0]["function"]["strict"], true);
        assert_eq!(
            result[0]["function"]["parameters"]["additionalProperties"],
            false
        );
    }

    #[test]
    fn test_convert_registry_no_tags_excludes_when_filter_active() {
        let converter = OpenAIConverter::new();
        let registry = json!({
            "ping": {
                "description": "Ping",
                "input_schema": {}
                // No tags field
            }
        });
        let result = converter
            .convert_registry_json(&registry, false, false, Some(&["needed"]), None)
            .unwrap();
        assert!(result.is_empty());
    }

    #[test]
    fn test_convert_registry_combined_tags_and_prefix() {
        let converter = OpenAIConverter::new();
        let registry = json!({
            "image.resize": {
                "description": "Resize",
                "input_schema": {},
                "tags": ["transform"]
            },
            "image.crop": {
                "description": "Crop",
                "input_schema": {},
                "tags": ["transform"]
            },
            "text.summarize": {
                "description": "Summarize",
                "input_schema": {},
                "tags": ["transform"]
            }
        });
        // Both prefix "image" and tag "transform"
        let result = converter
            .convert_registry_json(&registry, false, false, Some(&["transform"]), Some("image"))
            .unwrap();
        assert_eq!(result.len(), 2);
    }

    // ---- Task 5: integration tests ------------------------------------------

    #[test]
    fn test_e2e_simple_module() {
        let converter = OpenAIConverter::new();
        let registry = json!({
            "math.add": {
                "description": "Add two numbers",
                "input_schema": {
                    "type": "object",
                    "properties": {
                        "a": {"type": "number"},
                        "b": {"type": "number"}
                    },
                    "required": ["a", "b"]
                }
            }
        });
        let result = converter
            .convert_registry_json(&registry, false, false, None, None)
            .unwrap();
        assert_eq!(result.len(), 1);

        let tool = &result[0];
        assert_eq!(tool["type"], "function");
        assert_eq!(tool["function"]["name"], "math-add");
        assert_eq!(tool["function"]["description"], "Add two numbers");
        assert_eq!(tool["function"]["parameters"]["type"], "object");
        assert_eq!(
            tool["function"]["parameters"]["properties"]["a"],
            json!({"type": "number"})
        );
        assert_eq!(
            tool["function"]["parameters"]["properties"]["b"],
            json!({"type": "number"})
        );
        assert_eq!(
            tool["function"]["parameters"]["required"],
            json!(["a", "b"])
        );
        // No strict key when strict=false
        assert!(tool["function"].get("strict").is_none());
    }

    #[test]
    fn test_e2e_strict_mode_full() {
        let converter = OpenAIConverter::new();
        let registry = json!({
            "search.query": {
                "description": "Run a search",
                "input_schema": {
                    "type": "object",
                    "properties": {
                        "q": {
                            "type": "string",
                            "description": "Original desc",
                            "x-llm-description": "The search query to execute"
                        },
                        "limit": {
                            "type": "integer",
                            "default": 10,
                            "x-custom": "validation-hint"
                        }
                    },
                    "required": ["q"]
                }
            }
        });
        let result = converter
            .convert_registry_json(&registry, false, true, None, None)
            .unwrap();
        assert_eq!(result.len(), 1);

        let tool = &result[0];
        let func = &tool["function"];
        let params = &func["parameters"];

        // function.strict: true
        assert_eq!(func["strict"], true);

        // x-llm-description promoted to description
        assert_eq!(
            params["properties"]["q"]["description"],
            "The search query to execute"
        );
        // x-llm-description itself stripped
        assert!(params["properties"]["q"].get("x-llm-description").is_none());
        // x-custom stripped
        assert!(params["properties"]["limit"].get("x-custom").is_none());
        // default stripped
        assert!(params["properties"]["limit"].get("default").is_none());

        // additionalProperties: false
        assert_eq!(params["additionalProperties"], false);

        // All properties in required (sorted)
        assert_eq!(params["required"], json!(["limit", "q"]));

        // q was required -> NOT nullable
        assert_eq!(params["properties"]["q"]["type"], "string");
        // limit was optional -> nullable
        assert_eq!(
            params["properties"]["limit"]["type"],
            json!(["integer", "null"])
        );
    }

    #[test]
    fn test_e2e_annotations_embedded() {
        let converter = OpenAIConverter::new();
        let registry = json!({
            "fs.delete": {
                "description": "Delete a file",
                "input_schema": {
                    "type": "object",
                    "properties": {
                        "path": {"type": "string"}
                    },
                    "required": ["path"]
                },
                "annotations": {
                    "destructive": true
                }
            }
        });
        let result = converter
            .convert_registry_json(&registry, true, false, None, None)
            .unwrap();
        assert_eq!(result.len(), 1);

        let desc = result[0]["function"]["description"].as_str().unwrap();
        assert!(desc.starts_with("Delete a file"));
        assert!(desc.contains("WARNING"));
        assert!(desc.contains("DESTRUCTIVE"));
    }

    #[test]
    fn test_e2e_multiple_modules_filtered() {
        let converter = OpenAIConverter::new();
        let registry = json!({
            "image.resize": {
                "description": "Resize an image",
                "input_schema": {"type": "object", "properties": {"w": {"type": "integer"}}},
                "tags": ["image", "transform"]
            },
            "image.blur": {
                "description": "Blur an image",
                "input_schema": {"type": "object", "properties": {"r": {"type": "number"}}},
                "tags": ["image", "filter"]
            },
            "text.upper": {
                "description": "Uppercase text",
                "input_schema": {"type": "object", "properties": {"s": {"type": "string"}}},
                "tags": ["text"]
            }
        });

        // Filter by "image" tag -> 2 tools
        let result = converter
            .convert_registry_json(&registry, false, false, Some(&["image"]), None)
            .unwrap();
        assert_eq!(result.len(), 2);
        let names: Vec<&str> = result
            .iter()
            .map(|t| t["function"]["name"].as_str().unwrap())
            .collect();
        assert!(names.contains(&"image-blur"));
        assert!(names.contains(&"image-resize"));
        assert!(!names.contains(&"text-upper"));

        // Filter by "image" + "transform" -> only resize
        let result = converter
            .convert_registry_json(&registry, false, false, Some(&["image", "transform"]), None)
            .unwrap();
        assert_eq!(result.len(), 1);
        assert_eq!(result[0]["function"]["name"], "image-resize");
    }

    #[test]
    fn test_e2e_prefix_filter() {
        let converter = OpenAIConverter::new();
        let registry = json!({
            "image.resize": {
                "description": "Resize",
                "input_schema": {}
            },
            "image.crop": {
                "description": "Crop",
                "input_schema": {}
            },
            "audio.play": {
                "description": "Play",
                "input_schema": {}
            }
        });

        let result = converter
            .convert_registry_json(&registry, false, false, None, Some("image"))
            .unwrap();
        assert_eq!(result.len(), 2);
        for tool in &result {
            let name = tool["function"]["name"].as_str().unwrap();
            assert!(
                name.starts_with("image-"),
                "Expected image- prefix, got {}",
                name
            );
        }

        let result = converter
            .convert_registry_json(&registry, false, false, None, Some("audio"))
            .unwrap();
        assert_eq!(result.len(), 1);
        assert_eq!(result[0]["function"]["name"], "audio-play");
    }

    #[test]
    fn test_e2e_roundtrip_name_normalization() {
        let converter = OpenAIConverter::new();
        let registry = json!({
            "deep.nested.module.name": {
                "description": "A deeply nested module",
                "input_schema": {}
            }
        });
        let result = converter
            .convert_registry_json(&registry, false, false, None, None)
            .unwrap();
        assert_eq!(result.len(), 1);

        let tool_name = result[0]["function"]["name"].as_str().unwrap();
        assert_eq!(tool_name, "deep-nested-module-name");

        // Roundtrip: denormalize should recover the original dot-separated ID
        let recovered = ModuleIDNormalizer::denormalize(tool_name);
        assert_eq!(recovered, "deep.nested.module.name");
    }

    #[test]
    fn test_e2e_empty_schema() {
        let converter = OpenAIConverter::new();
        let registry = json!({
            "ping": {
                "description": "Health check",
                "input_schema": {}
            }
        });
        let result = converter
            .convert_registry_json(&registry, false, false, None, None)
            .unwrap();
        assert_eq!(result.len(), 1);
        assert_eq!(
            result[0]["function"]["parameters"],
            json!({"type": "object", "properties": {}})
        );
    }

    #[test]
    fn test_e2e_no_input_schema_field() {
        // Module descriptor with no input_schema key at all
        let converter = OpenAIConverter::new();
        let registry = json!({
            "noop": {
                "description": "Does nothing"
            }
        });
        let result = converter
            .convert_registry_json(&registry, false, false, None, None)
            .unwrap();
        assert_eq!(result.len(), 1);
        assert_eq!(
            result[0]["function"]["parameters"],
            json!({"type": "object", "properties": {}})
        );
    }

    #[test]
    fn test_e2e_empty_description() {
        // Module with empty description string
        let converter = OpenAIConverter::new();
        let registry = json!({
            "mystery": {
                "description": "",
                "input_schema": {}
            }
        });
        let result = converter
            .convert_registry_json(&registry, false, false, None, None)
            .unwrap();
        assert_eq!(result.len(), 1);
        assert_eq!(result[0]["function"]["description"], "");
    }

    // ---- Issue D11-024: convert_registry with live Registry ----------

    #[test]
    fn test_convert_registry_empty_registry() {
        // [D11-024] convert_registry must work with live apcore Registry.
        use apcore::registry::registry::Registry;
        use std::sync::Arc;

        let registry = Arc::new(Registry::default());
        let converter = OpenAIConverter::new();
        let result = converter
            .convert_registry(&registry, false, false, None, None)
            .unwrap();
        assert!(
            result.is_empty(),
            "empty registry must produce empty tools list"
        );
    }

    #[test]
    fn test_convert_registry_with_module() {
        // [D11-024] convert_registry enumerates live Registry and produces tools.
        use apcore::context::Context;
        use apcore::errors::ModuleError;
        use apcore::module::Module;
        use apcore::registry::{registry::Registry, ModuleDescriptor};
        use std::sync::Arc;

        #[derive(Debug)]
        struct Noop;
        #[async_trait::async_trait]
        impl Module for Noop {
            fn input_schema(&self) -> serde_json::Value {
                json!({"type":"object","properties":{}})
            }
            fn output_schema(&self) -> serde_json::Value {
                json!({"type":"object"})
            }
            fn description(&self) -> &str {
                "no-op"
            }
            async fn execute(
                &self,
                _: serde_json::Value,
                _: &Context<serde_json::Value>,
            ) -> Result<serde_json::Value, ModuleError> {
                Ok(json!({}))
            }
        }

        let registry = Arc::new(Registry::default());
        let descriptor = ModuleDescriptor {
            module_id: "math.add".to_string(),
            name: None,
            description: "Add two numbers".to_string(),
            documentation: None,
            input_schema: json!({"type":"object","properties":{"a":{"type":"number"},"b":{"type":"number"}},"required":["a","b"]}),
            output_schema: json!({"type":"object"}),
            version: "1.0.0".to_string(),
            tags: vec![],
            annotations: None,
            examples: vec![],
            metadata: std::collections::HashMap::new(),
            display: None,
            sunset_date: None,
            dependencies: vec![],
            enabled: true,
        };
        registry
            .register("math.add", Box::new(Noop), descriptor)
            .unwrap();

        let converter = OpenAIConverter::new();
        let result = converter
            .convert_registry(&registry, false, false, None, None)
            .unwrap();
        assert_eq!(result.len(), 1);
        assert_eq!(result[0]["function"]["name"], "math-add");
        assert_eq!(result[0]["function"]["description"], "Add two numbers");
    }

    // ---- Issue D11-019 partial: arguments format is JSON not debug repr -----

    #[test]
    fn test_approval_arguments_format_is_json() {
        // [D11-019] Confirming test: serde_json::Value::to_string produces JSON
        // format ({"key":"val"}), not Rust debug repr ({key: "val"}).
        // This test documents the correct behavior in the converter layer.
        let args = json!({"key": "val"});
        let formatted = args.to_string();
        assert!(
            formatted.contains("\"key\""),
            "arguments must be JSON-formatted: {formatted}"
        );
        assert!(
            formatted.contains("\"val\""),
            "arguments must be JSON-formatted: {formatted}"
        );
        assert!(
            !formatted.contains("key: "),
            "must NOT be Rust debug repr: {formatted}"
        );
    }

    // ---- rich_description (apcore-toolkit 0.6+ format_module integration) ---

    #[test]
    fn json_entry_to_scanned_module_copies_overlapping_fields() {
        // The JSON-entry adapter is the primitive that lets the
        // duck-typed `convert_registry` JSON path drive
        // `format_module(Markdown)`. Every field copied must round-trip.
        let entry = json!({
            "description": "Resize an image",
            "input_schema": {
                "type": "object",
                "properties": {"width": {"type": "integer"}},
                "required": ["width"]
            },
            "output_schema": {"type": "object"},
            "tags": ["image", "transform"],
            "version": "2.0.0",
            "annotations": {"idempotent": true},
            "documentation": "Long-form docs",
            "examples": [],
            "metadata": {"http_method": "POST"}
        });
        let scanned = json_entry_to_scanned_module("image.resize", &entry);
        assert_eq!(scanned.module_id, "image.resize");
        assert_eq!(scanned.description, "Resize an image");
        assert_eq!(scanned.tags, vec!["image", "transform"]);
        assert_eq!(scanned.version, "2.0.0");
        assert_eq!(scanned.documentation.as_deref(), Some("Long-form docs"));
        assert_eq!(
            scanned.metadata.get("http_method").and_then(|v| v.as_str()),
            Some("POST")
        );
    }

    #[test]
    fn json_entry_to_scanned_module_uses_defaults_for_missing_fields() {
        // Sparse entry — toolkit's markdown render handles empty
        // sections gracefully; the adapter just needs to not blow up.
        let entry = json!({
            "description": "Minimal",
            "input_schema": {"type": "object"}
        });
        let scanned = json_entry_to_scanned_module("min.module", &entry);
        assert_eq!(scanned.version, "1.0.0");
        assert!(scanned.tags.is_empty());
        assert!(scanned.examples.is_empty());
        assert!(scanned.documentation.is_none());
    }

    #[test]
    fn convert_descriptor_with_options_rich_description_renders_markdown() {
        // The JSON-path convert_descriptor with rich_description=true
        // must replace the supplied plain description with toolkit
        // Markdown — same byte-equivalent rendering the
        // ModuleDescriptor path produces.
        let converter = OpenAIConverter::new();
        let descriptor = json!({
            "description": "Resize an image",
            "input_schema": {
                "type": "object",
                "properties": {"width": {"type": "integer"}},
                "required": ["width"]
            },
            "tags": ["image"]
        });
        let tool = converter
            .convert_descriptor_with_options(
                "image.resize",
                &descriptor,
                "Resize an image",
                ConvertOptions::default().with_rich_description(true),
            )
            .expect("convert with rich_description");
        let desc = tool
            .get("function")
            .and_then(|f| f.get("description"))
            .and_then(|d| d.as_str())
            .unwrap_or("");
        assert!(
            desc.starts_with("# "),
            "rich description must be Markdown (start with '# '); got: {desc}"
        );
        assert!(
            desc.contains("## Parameters"),
            "Markdown must include the Parameters section; got: {desc}"
        );
        assert!(
            desc.contains("Resize an image"),
            "Markdown must embed the original description; got: {desc}"
        );
    }

    #[test]
    fn convert_registry_json_with_options_rich_description_propagates_to_every_tool() {
        // rich_description on the JSON-path convert_registry must
        // render Markdown for every emitted tool — proving
        // json_entry_to_scanned_module sees per-entry data.
        let converter = OpenAIConverter::new();
        let registry = json!({
            "demo.one": {
                "description": "First demo",
                "input_schema": {"type": "object"},
                "tags": []
            },
            "demo.two": {
                "description": "Second demo",
                "input_schema": {"type": "object"},
                "tags": []
            }
        });
        let tools = converter
            .convert_registry_json_with_options(
                &registry,
                ConvertOptions::default().with_rich_description(true),
                None,
                None,
            )
            .expect("convert_registry with rich_description");
        assert_eq!(tools.len(), 2);
        for tool in &tools {
            let desc = tool
                .get("function")
                .and_then(|f| f.get("description"))
                .and_then(|d| d.as_str())
                .unwrap_or("");
            assert!(
                desc.starts_with("# "),
                "every tool must have Markdown description; got: {desc}"
            );
        }
    }

    #[test]
    fn convert_registry_with_options_rich_description_uses_real_descriptor() {
        // The apcore-Registry path can lean on `markdown::render_module_markdown`
        // because it has the real `ModuleDescriptor` (incl. documentation,
        // examples, display overlay) — strictly richer than the JSON path.
        use apcore::context::Context;
        use apcore::module::{Module, ModuleAnnotations};
        use apcore::registry::registry::Registry;
        use apcore::registry::ModuleDescriptor;
        use async_trait::async_trait;
        use std::sync::Arc;

        #[derive(Debug)]
        struct DemoModule;

        #[async_trait]
        impl Module for DemoModule {
            fn input_schema(&self) -> Value {
                json!({"type": "object"})
            }
            fn output_schema(&self) -> Value {
                json!({"type": "object"})
            }
            fn description(&self) -> &str {
                "Resize an image"
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
        let descriptor = ModuleDescriptor {
            module_id: "image.resize".to_string(),
            name: None,
            description: "Resize an image".to_string(),
            documentation: Some("Long-form docs".to_string()),
            input_schema: json!({"type": "object"}),
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
            .expect("register");

        let converter = OpenAIConverter::new();
        let tools = converter
            .convert_registry_with_options(
                &registry,
                ConvertOptions::default().with_rich_description(true),
                None,
                None,
            )
            .expect("convert");
        assert_eq!(tools.len(), 1);
        let desc = tools[0]
            .get("function")
            .and_then(|f| f.get("description"))
            .and_then(|d| d.as_str())
            .unwrap_or("");
        assert!(desc.starts_with("# "), "apcore path must render Markdown");
        assert!(desc.contains("Resize an image"));
    }
}
