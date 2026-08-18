//! SchemaConverter — converts between apcore JSON schemas and MCP tool schemas.

use std::collections::HashSet;

use serde_json::{json, Value};

use super::AdapterError;

const MAX_REF_DEPTH: usize = 32;

/// Converts apcore module input/output schemas to MCP-compatible JSON Schema.
///
/// Key transformations:
/// - Empty/null schemas become `{"type": "object", "properties": {}}`
/// - `$ref` references are inlined recursively (up to depth 32)
/// - `$defs` is stripped from the final output
/// - Root schema is guaranteed to have `"type": "object"`
pub struct SchemaConverter;

impl SchemaConverter {
    /// Convert a module descriptor's input schema to an MCP tool input schema.
    ///
    /// Primary public API matching Python `convert_input_schema(descriptor)` and
    /// TS `convertInputSchema(descriptor, options?)` — takes the full descriptor.
    /// [D10-008]
    ///
    /// # Arguments
    /// * `descriptor` — the apcore module descriptor.
    /// * `strict` — if true, inject `additionalProperties: false` on all object nodes.
    pub fn convert_input_schema_descriptor(
        descriptor: &apcore::registry::ModuleDescriptor,
        strict: bool,
    ) -> Result<Value, AdapterError> {
        Self::convert_input_schema_strict(&descriptor.input_schema, strict)
    }

    /// Convert an apcore input schema to an MCP tool input schema.
    ///
    /// MCP requires `type: "object"` at the top level with explicit `properties`.
    /// Defaults to strict mode (injects `additionalProperties: false` on every
    /// object node lacking an explicit setting).
    pub fn convert_input_schema(schema: &Value) -> Result<Value, AdapterError> {
        Self::convert_schema_with(schema, true)
    }

    /// Convert an apcore output schema to an MCP-compatible output description.
    /// Defaults to strict mode.
    pub fn convert_output_schema(schema: &Value) -> Result<Value, AdapterError> {
        Self::convert_schema_with(schema, true)
    }

    /// Convert an input schema with explicit strictness control.
    pub fn convert_input_schema_strict(
        schema: &Value,
        strict: bool,
    ) -> Result<Value, AdapterError> {
        Self::convert_schema_with(schema, strict)
    }

    /// Convert an output schema with explicit strictness control.
    pub fn convert_output_schema_strict(
        schema: &Value,
        strict: bool,
    ) -> Result<Value, AdapterError> {
        Self::convert_schema_with(schema, strict)
    }

    fn convert_schema_with(schema: &Value, strict: bool) -> Result<Value, AdapterError> {
        // Clone to avoid mutating input
        let mut schema = schema.clone();

        // Handle null or empty object
        if schema.is_null() || schema.as_object().is_some_and(|m| m.is_empty()) {
            let mut base = json!({"type": "object", "properties": {}});
            if strict {
                base.as_object_mut()
                    .unwrap()
                    .insert("additionalProperties".to_string(), json!(false));
            }
            return Ok(base);
        }

        // Inline $refs unconditionally.
        //
        // [A-D-SC-1] Gating this on a root `$defs` let three things slip: a
        // dangling `{"$ref": "#/$defs/Foo"}` was returned verbatim as an
        // unresolvable pointer in the MCP inputSchema, nested `$defs` were not
        // stripped, and the MAX_REF_DEPTH cap — which lives inside the inliner —
        // went unenforced. TypeScript always inlines (schema.ts:68) and throws
        // `$ref not found`; an absent `$defs` is simply an empty definition map.
        let defs = schema.get("$defs").cloned().unwrap_or_else(|| json!({}));
        schema = Self::inline_refs(&schema, &defs, &HashSet::new(), 0)?;
        if let Some(obj) = schema.as_object_mut() {
            obj.remove("$defs");
        }

        // Ensure root type: object
        Self::ensure_object_type(&mut schema);

        if strict {
            Self::inject_strict(&mut schema);
        }

        Ok(schema)
    }

    /// Recursively inject `additionalProperties: false` on every object node
    /// that doesn't already have an explicit `additionalProperties` setting.
    /// Preserves user-set `additionalProperties: true` (or any other value).
    ///
    /// [SC-9] Walks ONLY a whitelist of subschema-bearing keys (mirrors
    /// Python's `_SCHEMA_CHILD_*` sets and TS's recursion contract).
    /// Pre-fix Rust descended into every map key — including `enum`,
    /// `const`, `examples`, `default`, `required` — and could spuriously
    /// inject `additionalProperties:false` on object-shaped values inside
    /// those data leaves.
    ///
    /// [SC-18] Object-type detection accepts both `type: "object"` and
    /// `type: ["object", "null"]` (nullable-object form). Pre-fix Rust
    /// only matched the bare-string form; nullable-object schemas with
    /// `properties` would also be incorrectly downgraded.
    fn inject_strict(node: &mut Value) {
        // Subschema-bearing keys whose VALUES contain schemas (recurse into).
        const SUBSCHEMA_DICT_KEYS: &[&str] =
            &["properties", "patternProperties", "$defs", "definitions"];
        // Keys whose value is a single nested schema.
        //
        // [A-D-SC-2] `propertyNames` was missing from this set, so an object
        // nested under it shipped without `additionalProperties: false` from
        // Rust and with it from Python (_SCHEMA_CHILD_SCHEMA_KEYS) and
        // TypeScript, both of which walk the full spec-pinned set.
        const SUBSCHEMA_KEYS: &[&str] = &[
            "items",
            "additionalProperties",
            "not",
            "if",
            "then",
            "else",
            "contains",
            "propertyNames",
        ];
        // Keys whose value is a list of nested schemas.
        // [A-D-SC-2] `prefixItems` likewise.
        const SUBSCHEMA_LIST_KEYS: &[&str] = &["oneOf", "anyOf", "allOf", "prefixItems"];

        // [A-D-SC-2] An array-valued `items` (legacy tuple validation) arrives
        // here as a `Value::Array`; with no arm for it the whole tuple was a
        // silent no-op.
        if let Value::Array(arr) = node {
            for v in arr.iter_mut() {
                Self::inject_strict(v);
            }
            return;
        }

        // Strict-mode otherwise applies to schema objects only.
        if let Value::Object(map) = node {
            // [SC-18] Detect object type with nullable form support.
            let type_val = map.get("type");
            let is_object_type = match type_val {
                Some(Value::String(s)) => s == "object",
                Some(Value::Array(arr)) => arr.iter().any(|v| v.as_str() == Some("object")),
                None => map.contains_key("properties"),
                _ => false,
            };
            if is_object_type && !map.contains_key("additionalProperties") {
                map.insert("additionalProperties".to_string(), json!(false));
            }
            // [SC-9] Recurse only into whitelisted subschema slots.
            // Skip enum, const, examples, default, required, type, etc.
            for &key in SUBSCHEMA_DICT_KEYS {
                if let Some(Value::Object(inner)) = map.get_mut(key) {
                    for (_, v) in inner.iter_mut() {
                        Self::inject_strict(v);
                    }
                }
            }
            for &key in SUBSCHEMA_KEYS {
                if let Some(v) = map.get_mut(key) {
                    Self::inject_strict(v);
                }
            }
            for &key in SUBSCHEMA_LIST_KEYS {
                if let Some(Value::Array(arr)) = map.get_mut(key) {
                    for v in arr.iter_mut() {
                        Self::inject_strict(v);
                    }
                }
            }
        }
    }

    fn inline_refs(
        schema: &Value,
        defs: &Value,
        seen: &HashSet<String>,
        depth: usize,
    ) -> Result<Value, AdapterError> {
        if depth > MAX_REF_DEPTH {
            return Err(AdapterError::SchemaConversion(format!(
                "$ref resolution exceeded maximum depth of {MAX_REF_DEPTH}"
            )));
        }

        match schema {
            Value::Object(map) => {
                // If this is a $ref, resolve it
                if let Some(ref_val) = map.get("$ref") {
                    let ref_path = ref_val.as_str().ok_or_else(|| {
                        AdapterError::SchemaConversion("$ref value must be a string".into())
                    })?;

                    if seen.contains(ref_path) {
                        return Err(AdapterError::SchemaConversion(format!(
                            "Circular $ref detected: {ref_path}"
                        )));
                    }

                    let mut new_seen = seen.clone();
                    new_seen.insert(ref_path.to_string());

                    let resolved = Self::resolve_ref(ref_path, defs)?;
                    return Self::inline_refs(&resolved, defs, &new_seen, depth + 1);
                }

                // Otherwise, recursively process all values
                let mut result = serde_json::Map::new();
                for (key, value) in map {
                    if key == "$defs" {
                        continue;
                    }
                    result.insert(
                        key.clone(),
                        Self::inline_refs(value, defs, seen, depth + 1)?,
                    );
                }
                Ok(Value::Object(result))
            }
            Value::Array(arr) => {
                let items: Result<Vec<Value>, _> = arr
                    .iter()
                    .map(|item| Self::inline_refs(item, defs, seen, depth + 1))
                    .collect();
                Ok(Value::Array(items?))
            }
            // Primitive value, return as-is
            other => Ok(other.clone()),
        }
    }

    fn resolve_ref(ref_path: &str, defs: &Value) -> Result<Value, AdapterError> {
        if !ref_path.starts_with("#/$defs/") {
            return Err(AdapterError::SchemaConversion(format!(
                "Unsupported $ref format: {ref_path}"
            )));
        }

        let def_name = &ref_path[8..]; // Remove "#/$defs/"

        let defs_map = defs
            .as_object()
            .ok_or_else(|| AdapterError::SchemaConversion("$defs must be an object".into()))?;

        defs_map.get(def_name).cloned().ok_or_else(|| {
            AdapterError::SchemaConversion(format!("Definition not found: {def_name}"))
        })
    }

    fn ensure_object_type(schema: &mut Value) {
        if let Some(map) = schema.as_object_mut() {
            if !map.contains_key("type") {
                map.insert("type".to_string(), json!("object"));
                return;
            }
            // [SC-18] When `properties` is present but `type` is set to
            // something non-object-shaped, force `type: "object"`.
            // CRITICALLY: preserve `type: ["object", "null"]` and other
            // list-form types that include "object" — pre-fix Rust did
            // a strict-equality check against `json!("object")` which
            // caused nullable-object schemas to be downgraded to bare
            // object, losing the nullable signal.
            if map.contains_key("properties") {
                let already_object = match map.get("type") {
                    Some(Value::String(s)) => s == "object",
                    Some(Value::Array(arr)) => arr.iter().any(|v| v.as_str() == Some("object")),
                    _ => false,
                };
                if !already_object {
                    map.insert("type".to_string(), json!("object"));
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn test_empty_schema() {
        let schema = json!({});
        let result = SchemaConverter::convert_input_schema_strict(&schema, false).unwrap();
        assert_eq!(result, json!({"type": "object", "properties": {}}));
    }

    #[test]
    fn test_null_schema() {
        let result = SchemaConverter::convert_input_schema_strict(&Value::Null, false).unwrap();
        assert_eq!(result, json!({"type": "object", "properties": {}}));
    }

    #[test]
    fn test_simple_object_passthrough() {
        let schema = json!({
            "type": "object",
            "properties": {
                "name": {"type": "string"}
            }
        });
        let result = SchemaConverter::convert_input_schema_strict(&schema, false).unwrap();
        assert_eq!(result, schema);
    }

    #[test]
    fn test_missing_type_gets_object() {
        let schema = json!({
            "properties": {
                "name": {"type": "string"}
            }
        });
        let result = SchemaConverter::convert_input_schema(&schema).unwrap();
        assert_eq!(result["type"], "object");
        assert_eq!(result["properties"]["name"]["type"], "string");
    }

    #[test]
    fn test_inline_simple_ref() {
        let schema = json!({
            "type": "object",
            "properties": {
                "step": {"$ref": "#/$defs/Step"}
            },
            "$defs": {
                "Step": {
                    "type": "object",
                    "properties": {
                        "id": {"type": "integer"}
                    }
                }
            }
        });
        let result = SchemaConverter::convert_input_schema(&schema).unwrap();
        assert_eq!(result["properties"]["step"]["type"], "object");
        assert_eq!(
            result["properties"]["step"]["properties"]["id"]["type"],
            "integer"
        );
        assert!(result.get("$defs").is_none());
    }

    #[test]
    fn test_inline_nested_refs() {
        let schema = json!({
            "type": "object",
            "properties": {
                "outer": {"$ref": "#/$defs/Outer"}
            },
            "$defs": {
                "Outer": {
                    "type": "object",
                    "properties": {
                        "inner": {"$ref": "#/$defs/Inner"}
                    }
                },
                "Inner": {
                    "type": "object",
                    "properties": {
                        "value": {"type": "string"}
                    }
                }
            }
        });
        let result = SchemaConverter::convert_input_schema(&schema).unwrap();
        assert_eq!(
            result["properties"]["outer"]["properties"]["inner"]["properties"]["value"]["type"],
            "string"
        );
        assert!(result.get("$defs").is_none());
    }

    #[test]
    fn test_inline_ref_in_properties() {
        let schema = json!({
            "type": "object",
            "properties": {
                "config": {"$ref": "#/$defs/Config"}
            },
            "$defs": {
                "Config": {
                    "type": "object",
                    "properties": {
                        "enabled": {"type": "boolean"}
                    }
                }
            }
        });
        let result = SchemaConverter::convert_input_schema(&schema).unwrap();
        assert_eq!(
            result["properties"]["config"]["properties"]["enabled"]["type"],
            "boolean"
        );
    }

    #[test]
    fn test_inline_ref_in_array_items() {
        let schema = json!({
            "type": "object",
            "properties": {
                "steps": {
                    "type": "array",
                    "items": {"$ref": "#/$defs/Step"}
                }
            },
            "$defs": {
                "Step": {
                    "type": "object",
                    "properties": {
                        "name": {"type": "string"}
                    }
                }
            }
        });
        let result = SchemaConverter::convert_input_schema(&schema).unwrap();
        assert_eq!(result["properties"]["steps"]["items"]["type"], "object");
        assert_eq!(
            result["properties"]["steps"]["items"]["properties"]["name"]["type"],
            "string"
        );
    }

    #[test]
    fn test_defs_stripped_after_inlining() {
        let schema = json!({
            "type": "object",
            "properties": {
                "x": {"$ref": "#/$defs/X"}
            },
            "$defs": {
                "X": {"type": "string"}
            }
        });
        let result = SchemaConverter::convert_input_schema(&schema).unwrap();
        assert!(result.get("$defs").is_none());
    }

    #[test]
    fn test_circular_ref_detected() {
        let schema = json!({
            "type": "object",
            "properties": {
                "node": {"$ref": "#/$defs/Node"}
            },
            "$defs": {
                "Node": {
                    "type": "object",
                    "properties": {
                        "child": {"$ref": "#/$defs/Node"}
                    }
                }
            }
        });
        let result = SchemaConverter::convert_input_schema(&schema);
        assert!(result.is_err());
        let err = result.unwrap_err().to_string();
        assert!(err.contains("Circular $ref detected"));
    }

    #[test]
    fn test_depth_exceeded() {
        // Build a chain of 34 $defs, each referencing the next
        let mut defs = serde_json::Map::new();
        for i in 0..34 {
            let next = if i < 33 {
                json!({"$ref": format!("#/$defs/D{}", i + 1)})
            } else {
                json!({"type": "string"})
            };
            defs.insert(format!("D{i}"), next);
        }
        let schema = json!({
            "type": "object",
            "properties": {
                "x": {"$ref": "#/$defs/D0"}
            },
            "$defs": Value::Object(defs)
        });
        let result = SchemaConverter::convert_input_schema(&schema);
        assert!(result.is_err());
        let err = result.unwrap_err().to_string();
        assert!(err.contains("exceeded maximum depth"));
    }

    #[test]
    fn test_diamond_ref_allowed() {
        let schema = json!({
            "type": "object",
            "properties": {
                "a": {"$ref": "#/$defs/Shared"},
                "b": {"$ref": "#/$defs/Shared"}
            },
            "$defs": {
                "Shared": {
                    "type": "object",
                    "properties": {
                        "val": {"type": "integer"}
                    }
                }
            }
        });
        let result = SchemaConverter::convert_input_schema(&schema).unwrap();
        assert_eq!(
            result["properties"]["a"]["properties"]["val"]["type"],
            "integer"
        );
        assert_eq!(
            result["properties"]["b"]["properties"]["val"]["type"],
            "integer"
        );
    }

    #[test]
    fn test_ref_not_found() {
        let schema = json!({
            "type": "object",
            "properties": {
                "x": {"$ref": "#/$defs/Missing"}
            },
            "$defs": {}
        });
        let result = SchemaConverter::convert_input_schema(&schema);
        assert!(result.is_err());
        let err = result.unwrap_err().to_string();
        assert!(err.contains("Definition not found: Missing"));
    }

    #[test]
    fn test_unsupported_ref_format() {
        let schema = json!({
            "type": "object",
            "properties": {
                "x": {"$ref": "http://example.com/schema"}
            },
            "$defs": {}
        });
        let result = SchemaConverter::convert_input_schema(&schema);
        assert!(result.is_err());
        let err = result.unwrap_err().to_string();
        assert!(err.contains("Unsupported $ref format"));
    }

    #[test]
    fn test_preserves_additional_properties() {
        let schema = json!({
            "type": "object",
            "properties": {
                "name": {"type": "string"}
            },
            "required": ["name"],
            "additionalProperties": false
        });
        let result = SchemaConverter::convert_input_schema(&schema).unwrap();
        assert_eq!(result["required"], json!(["name"]));
        assert_eq!(result["additionalProperties"], json!(false));
    }

    #[test]
    fn test_convert_input_schema() {
        let schema = json!({
            "type": "object",
            "properties": {
                "query": {"type": "string"},
                "config": {"$ref": "#/$defs/Config"}
            },
            "required": ["query"],
            "$defs": {
                "Config": {
                    "type": "object",
                    "properties": {
                        "limit": {"type": "integer"}
                    }
                }
            }
        });
        let result = SchemaConverter::convert_input_schema(&schema).unwrap();
        assert_eq!(result["type"], "object");
        assert_eq!(result["properties"]["query"]["type"], "string");
        assert_eq!(
            result["properties"]["config"]["properties"]["limit"]["type"],
            "integer"
        );
        assert_eq!(result["required"], json!(["query"]));
        assert!(result.get("$defs").is_none());
    }

    #[test]
    fn test_convert_output_schema() {
        let schema = json!({
            "type": "object",
            "properties": {
                "result": {"type": "string"},
                "metadata": {"$ref": "#/$defs/Meta"}
            },
            "$defs": {
                "Meta": {
                    "type": "object",
                    "properties": {
                        "timestamp": {"type": "string"}
                    }
                }
            }
        });
        let result = SchemaConverter::convert_output_schema(&schema).unwrap();
        assert_eq!(result["type"], "object");
        assert_eq!(result["properties"]["result"]["type"], "string");
        assert_eq!(
            result["properties"]["metadata"]["properties"]["timestamp"]["type"],
            "string"
        );
        assert!(result.get("$defs").is_none());
    }

    #[test]
    fn test_strict_root_additional_properties_false() {
        let schema = json!({
            "type": "object",
            "properties": {"x": {"type": "string"}}
        });
        let result = SchemaConverter::convert_input_schema(&schema).unwrap();
        assert_eq!(result["additionalProperties"], json!(false));
    }

    #[test]
    fn test_strict_nested_additional_properties_false() {
        let schema = json!({
            "type": "object",
            "properties": {
                "inner": {
                    "type": "object",
                    "properties": {"y": {"type": "integer"}}
                }
            }
        });
        let result = SchemaConverter::convert_input_schema(&schema).unwrap();
        assert_eq!(result["additionalProperties"], json!(false));
        assert_eq!(
            result["properties"]["inner"]["additionalProperties"],
            json!(false)
        );
    }

    #[test]
    fn test_strict_preserves_user_additional_properties_true() {
        let schema = json!({
            "type": "object",
            "properties": {
                "open": {
                    "type": "object",
                    "properties": {},
                    "additionalProperties": true
                }
            }
        });
        let result = SchemaConverter::convert_input_schema(&schema).unwrap();
        // Root gets strict
        assert_eq!(result["additionalProperties"], json!(false));
        // User-set true preserved
        assert_eq!(
            result["properties"]["open"]["additionalProperties"],
            json!(true)
        );
    }

    #[test]
    fn test_original_not_mutated() {
        let schema = json!({
            "type": "object",
            "properties": {
                "x": {"$ref": "#/$defs/X"}
            },
            "$defs": {
                "X": {"type": "string"}
            }
        });
        let original = schema.clone();
        let _ = SchemaConverter::convert_input_schema(&schema).unwrap();
        assert_eq!(schema, original, "Original schema must not be mutated");
    }

    /// [SC-18] Regression: nullable-object schemas (`type: ["object",
    /// "null"]`) must NOT be downgraded to bare `type: "object"` by
    /// ensure_object_type. The list form is valid JSON Schema and signals
    /// nullability; downgrading loses that signal.
    #[test]
    fn nullable_object_type_preserved() {
        let schema = json!({
            "type": ["object", "null"],
            "properties": {
                "name": {"type": "string"}
            }
        });
        let result = SchemaConverter::convert_input_schema(&schema).unwrap();
        // The type field must remain a list including "object" and "null".
        let type_field = result.get("type").expect("type must be present");
        match type_field {
            Value::Array(arr) => {
                let strs: Vec<&str> = arr.iter().filter_map(|v| v.as_str()).collect();
                assert!(strs.contains(&"object"), "type must include 'object'");
                assert!(strs.contains(&"null"), "type must include 'null'");
            }
            other => panic!("expected type to remain a list, got: {other:?}"),
        }
    }

    /// [D10-008] Descriptor-level API: convert_input_schema_descriptor takes
    /// the full descriptor and reads descriptor.input_schema automatically.
    #[test]
    fn test_convert_input_schema_descriptor() {
        use apcore::registry::ModuleDescriptor;
        use std::collections::HashMap;
        let descriptor = ModuleDescriptor {
            module_id: "test.module".to_string(),
            name: None,
            description: "test".to_string(),
            documentation: None,
            input_schema: json!({
                "type": "object",
                "properties": {
                    "name": {"type": "string"}
                }
            }),
            output_schema: json!({}),
            version: "1.0.0".to_string(),
            tags: vec![],
            annotations: None,
            examples: vec![],
            metadata: HashMap::new(),
            display: None,
            sunset_date: None,
            dependencies: vec![],
            enabled: true,
        };
        let result = SchemaConverter::convert_input_schema_descriptor(&descriptor, false).unwrap();
        assert_eq!(result["type"], "object");
        assert_eq!(result["properties"]["name"]["type"], "string");
    }

    /// [SC-9] Regression: strict-mode injection must NOT descend into
    /// `enum`, `const`, `examples`, or `default` arrays/objects, even if
    /// they happen to contain object-shaped values. Pre-fix Rust walked
    /// every map key and would spuriously inject additionalProperties
    /// into data leaves.
    #[test]
    fn strict_mode_does_not_descend_into_enum_const_examples() {
        let schema = json!({
            "type": "object",
            "properties": {
                "color": {
                    "type": "object",
                    // enum entries are object-shaped DATA, not subschemas.
                    "enum": [
                        {"properties": {"r": {"type": "number"}}},
                        {"properties": {"g": {"type": "number"}}}
                    ],
                    // examples likewise are data, not subschemas.
                    "examples": [
                        {"properties": {"hex": "#fff"}}
                    ]
                }
            }
        });
        let result = SchemaConverter::convert_input_schema(&schema).unwrap();
        // The enum/examples entries should NOT have additionalProperties
        // injected. Walk into them and assert.
        let color = result
            .get("properties")
            .and_then(|p| p.get("color"))
            .unwrap();
        let enum_arr = color.get("enum").and_then(|v| v.as_array()).unwrap();
        for entry in enum_arr {
            assert!(
                entry.get("additionalProperties").is_none(),
                "strict must NOT inject into enum data leaves; got: {entry:?}"
            );
        }
        let examples_arr = color.get("examples").and_then(|v| v.as_array()).unwrap();
        for entry in examples_arr {
            assert!(
                entry.get("additionalProperties").is_none(),
                "strict must NOT inject into examples data leaves; got: {entry:?}"
            );
        }
    }

    // ---- [A-D-SC-1] dangling $ref must not leak ----

    /// A `$ref` with no root `$defs` used to skip the inliner entirely and
    /// ship an unresolvable pointer in the MCP inputSchema. TypeScript always
    /// inlines and throws `$ref not found`.
    #[test]
    fn test_dangling_ref_without_defs_is_an_error() {
        let schema = json!({"$ref": "#/$defs/Foo"});
        let err = SchemaConverter::convert_input_schema(&schema)
            .expect_err("a dangling $ref must not be returned verbatim");
        let msg = err.to_string();
        assert!(
            msg.contains("Foo"),
            "error should name the missing definition; got: {msg}"
        );
    }

    /// The same applies one level down, inside `properties`.
    #[test]
    fn test_nested_dangling_ref_without_defs_is_an_error() {
        let schema = json!({
            "type": "object",
            "properties": {"a": {"$ref": "#/$defs/Missing"}}
        });
        assert!(SchemaConverter::convert_input_schema(&schema).is_err());
    }

    /// Nested `$defs` are stripped at every level, not just the root.
    #[test]
    fn test_nested_defs_are_stripped_without_root_defs() {
        let schema = json!({
            "type": "object",
            "properties": {
                "a": {"type": "object", "properties": {}, "$defs": {"Unused": {"type": "string"}}}
            }
        });
        let result = SchemaConverter::convert_input_schema(&schema).expect("convert");
        assert!(
            result.pointer("/properties/a/$defs").is_none(),
            "nested $defs must be stripped; got: {result}"
        );
    }

    // ---- [A-D-SC-2] full spec-pinned recursion set ----

    /// `prefixItems` holds a LIST of subschemas. It was absent from every
    /// recursion set, so a nested object inside it shipped without
    /// `additionalProperties: false` from Rust and with it from the peers.
    #[test]
    fn test_strict_recurses_into_prefix_items() {
        let schema = json!({
            "type": "object",
            "properties": {
                "pair": {
                    "type": "array",
                    "prefixItems": [
                        {"type": "object", "properties": {"a": {"type": "string"}}},
                        {"type": "string"}
                    ]
                }
            }
        });
        let result = SchemaConverter::convert_input_schema(&schema).expect("convert");
        assert_eq!(
            result.pointer("/properties/pair/prefixItems/0/additionalProperties"),
            Some(&json!(false)),
            "strict must recurse into prefixItems; got: {result}"
        );
    }

    /// `propertyNames` holds a SINGLE subschema and was likewise unwalked.
    #[test]
    fn test_strict_recurses_into_property_names() {
        let schema = json!({
            "type": "object",
            "properties": {
                "map": {
                    "type": "object",
                    "propertyNames": {"type": "object", "properties": {"a": {"type": "string"}}}
                }
            }
        });
        let result = SchemaConverter::convert_input_schema(&schema).expect("convert");
        assert_eq!(
            result.pointer("/properties/map/propertyNames/additionalProperties"),
            Some(&json!(false)),
            "strict must recurse into propertyNames; got: {result}"
        );
    }

    /// An array-valued `items` (legacy tuple validation) reached a
    /// `Value::Array`, which `inject_strict` had no arm for — a silent no-op.
    #[test]
    fn test_strict_recurses_into_array_valued_items() {
        let schema = json!({
            "type": "object",
            "properties": {
                "tuple": {
                    "type": "array",
                    "items": [
                        {"type": "object", "properties": {"a": {"type": "string"}}},
                        {"type": "integer"}
                    ]
                }
            }
        });
        let result = SchemaConverter::convert_input_schema(&schema).expect("convert");
        assert_eq!(
            result.pointer("/properties/tuple/items/0/additionalProperties"),
            Some(&json!(false)),
            "strict must walk array-valued items; got: {result}"
        );
    }
}
