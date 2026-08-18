//! AnnotationMapper — converts apcore module annotations to MCP tool annotations.

use apcore::module::ModuleAnnotations;
use serde::{Deserialize, Serialize};

/// MCP tool annotations with camelCase field names for JSON serialization.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct McpAnnotations {
    pub read_only_hint: bool,
    pub destructive_hint: bool,
    pub idempotent_hint: bool,
    pub open_world_hint: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
}

/// Maps apcore module annotations to MCP-compatible tool annotations.
pub struct AnnotationMapper;

impl AnnotationMapper {
    /// Convert apcore annotations to MCP tool annotations.
    ///
    /// When `annotations` is `None`, returns sensible defaults:
    /// `readOnlyHint: false`, `destructiveHint: false`, `idempotentHint: false`,
    /// `openWorldHint: true`, `title: null`.
    pub fn to_mcp_annotations(annotations: Option<&ModuleAnnotations>) -> McpAnnotations {
        match annotations {
            None => McpAnnotations {
                read_only_hint: false,
                destructive_hint: false,
                idempotent_hint: false,
                open_world_hint: true,
                title: None,
            },
            Some(a) => McpAnnotations {
                read_only_hint: a.readonly,
                destructive_hint: a.destructive,
                idempotent_hint: a.idempotent,
                open_world_hint: a.open_world,
                title: None,
            },
        }
    }

    /// Generate annotation text to append to tool descriptions.
    ///
    /// Produces two sections:
    /// 1. Safety warnings for destructive/approval operations.
    /// 2. Machine-readable annotation block for non-default values.
    ///
    /// Returns an empty string if annotations is `None` or all values are defaults.
    pub fn to_description_suffix(annotations: Option<&ModuleAnnotations>) -> String {
        let annotations = match annotations {
            None => return String::new(),
            Some(a) => a,
        };

        let defaults = ModuleAnnotations::default();

        let mut warnings: Vec<String> = Vec::new();
        if annotations.destructive {
            warnings.push(
                "WARNING: DESTRUCTIVE - This operation may irreversibly modify or \
                 delete data. Confirm with user before calling."
                    .to_string(),
            );
        }
        if annotations.requires_approval {
            warnings.push(
                "REQUIRES APPROVAL: Human confirmation is required before execution.".to_string(),
            );
        }

        let mut parts: Vec<String> = Vec::new();
        if annotations.readonly != defaults.readonly {
            parts.push(format!("readonly={}", annotations.readonly));
        }
        if annotations.destructive != defaults.destructive {
            parts.push(format!("destructive={}", annotations.destructive));
        }
        if annotations.idempotent != defaults.idempotent {
            parts.push(format!("idempotent={}", annotations.idempotent));
        }
        if annotations.requires_approval != defaults.requires_approval {
            parts.push(format!(
                "requires_approval={}",
                annotations.requires_approval
            ));
        }
        if annotations.open_world != defaults.open_world {
            parts.push(format!("open_world={}", annotations.open_world));
        }
        if annotations.streaming != defaults.streaming {
            parts.push(format!("streaming={}", annotations.streaming));
        }
        if annotations.cacheable != defaults.cacheable {
            parts.push(format!("cacheable={}", annotations.cacheable));
        }
        if annotations.cache_ttl != defaults.cache_ttl {
            parts.push(format!("cache_ttl={}", annotations.cache_ttl));
        }
        if annotations.cache_key_fields != defaults.cache_key_fields {
            if let Some(fields) = &annotations.cache_key_fields {
                parts.push(format!("cache_key_fields=[{}]", fields.join(",")));
            }
        }
        if annotations.paginated != defaults.paginated {
            parts.push(format!("paginated={}", annotations.paginated));
        }
        if annotations.pagination_style != defaults.pagination_style {
            parts.push(format!("pagination_style={}", annotations.pagination_style));
        }

        // F-041 / D11-021 annotation metadata passthrough: surface any `mcp_`
        // prefixed extension keys from `annotations.extra`. Keys are sorted so
        // the rendered output is stable across runs.
        //
        // [D11-021] Align with Python's canonical format:
        //   1. Strip the `mcp_` prefix from keys.
        //   2. Format as `{stripped}: {value}` (colon+space separator, not equals).
        //   3. Emit each stripped extra as its own SEPARATE section OUTSIDE
        //      the `[Annotations: ...]` block, joined by `\n\n`.
        // This differs from the previous Rust format (key=value inside
        // [Annotations:...]) and from TS (joined by \n in one block).
        //
        // [A-D-AM-1] Only STRING values pass through. The spec restricts the
        // passthrough to keys "whose value is a string", Python guards with
        // `isinstance(extra[key], str)` and TypeScript with
        // `typeof value === "string"`. Filtering on the key prefix alone let
        // numbers, booleans and objects be JSON-serialized into the LLM-facing
        // tool description on Rust only.
        let mut mcp_extras: Vec<(&String, &str)> = annotations
            .extra
            .iter()
            .filter(|(k, _)| k.starts_with("mcp_"))
            .filter_map(|(k, v)| v.as_str().map(|s| (k, s)))
            .collect();
        mcp_extras.sort_by(|a, b| a.0.cmp(b.0));

        if warnings.is_empty() && parts.is_empty() && mcp_extras.is_empty() {
            return String::new();
        }

        let mut sections: Vec<String> = Vec::new();
        if !warnings.is_empty() {
            sections.push(warnings.join("\n"));
        }
        if !parts.is_empty() {
            sections.push(format!("[Annotations: {}]", parts.join(", ")));
        }
        // Each mcp_ extra is its own paragraph-level section, outside [Annotations:].
        for (k, v) in &mcp_extras {
            let stripped = k.strip_prefix("mcp_").unwrap_or(k);
            sections.push(format!("{stripped}: {v}"));
        }

        format!("\n\n{}", sections.join("\n\n"))
    }

    /// Check if module requires human approval before execution.
    ///
    /// Returns `false` if annotations is `None`.
    pub fn has_requires_approval(annotations: Option<&ModuleAnnotations>) -> bool {
        match annotations {
            None => false,
            Some(a) => a.requires_approval,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // ---- to_mcp_annotations tests ----

    #[test]
    fn test_to_mcp_annotations_none() {
        let result = AnnotationMapper::to_mcp_annotations(None);
        assert_eq!(
            result,
            McpAnnotations {
                read_only_hint: false,
                destructive_hint: false,
                idempotent_hint: false,
                open_world_hint: true,
                title: None,
            }
        );
    }

    #[test]
    fn test_to_mcp_annotations_readonly() {
        let ann = ModuleAnnotations {
            readonly: true,
            ..Default::default()
        };
        let result = AnnotationMapper::to_mcp_annotations(Some(&ann));
        assert!(result.read_only_hint);
        assert!(!result.destructive_hint);
    }

    #[test]
    fn test_to_mcp_annotations_destructive() {
        let ann = ModuleAnnotations {
            destructive: true,
            ..Default::default()
        };
        let result = AnnotationMapper::to_mcp_annotations(Some(&ann));
        assert!(result.destructive_hint);
        assert!(!result.read_only_hint);
    }

    #[test]
    fn test_to_mcp_annotations_all_set() {
        let ann = ModuleAnnotations {
            readonly: true,
            destructive: true,
            idempotent: true,
            open_world: false,
            ..Default::default()
        };
        let result = AnnotationMapper::to_mcp_annotations(Some(&ann));
        assert_eq!(
            result,
            McpAnnotations {
                read_only_hint: true,
                destructive_hint: true,
                idempotent_hint: true,
                open_world_hint: false,
                title: None,
            }
        );
    }

    #[test]
    fn test_to_mcp_annotations_serializes_camelcase() {
        let result = AnnotationMapper::to_mcp_annotations(None);
        let json = serde_json::to_value(&result).unwrap();
        assert!(json.get("readOnlyHint").is_some());
        assert!(json.get("destructiveHint").is_some());
        assert!(json.get("idempotentHint").is_some());
        assert!(json.get("openWorldHint").is_some());
        // title should be absent when None due to skip_serializing_if
        assert!(json.get("title").is_none());
    }

    // ---- to_description_suffix tests ----

    #[test]
    fn test_to_description_suffix_none() {
        let result = AnnotationMapper::to_description_suffix(None);
        assert_eq!(result, "");
    }

    #[test]
    fn test_to_description_suffix_destructive() {
        let ann = ModuleAnnotations {
            destructive: true,
            ..Default::default()
        };
        let result = AnnotationMapper::to_description_suffix(Some(&ann));
        assert!(result.contains("DESTRUCTIVE"));
        assert!(result.contains("WARNING"));
    }

    #[test]
    fn test_to_description_suffix_requires_approval() {
        let ann = ModuleAnnotations {
            requires_approval: true,
            ..Default::default()
        };
        let result = AnnotationMapper::to_description_suffix(Some(&ann));
        assert!(result.contains("REQUIRES APPROVAL"));
    }

    #[test]
    fn test_to_description_suffix_non_default_values() {
        let ann = ModuleAnnotations {
            readonly: true,
            streaming: true,
            ..Default::default()
        };
        let result = AnnotationMapper::to_description_suffix(Some(&ann));
        assert!(result.contains("[Annotations:"));
        assert!(result.contains("readonly=true"));
        assert!(result.contains("streaming=true"));
    }

    #[test]
    fn test_to_description_suffix_no_changes() {
        let ann = ModuleAnnotations::default();
        let result = AnnotationMapper::to_description_suffix(Some(&ann));
        assert_eq!(result, "");
    }

    #[test]
    fn test_to_description_suffix_destructive_and_approval() {
        let ann = ModuleAnnotations {
            destructive: true,
            requires_approval: true,
            ..Default::default()
        };
        let result = AnnotationMapper::to_description_suffix(Some(&ann));
        assert!(result.contains("DESTRUCTIVE"));
        assert!(result.contains("REQUIRES APPROVAL"));
        assert!(result.contains("[Annotations:"));
        assert!(result.contains("destructive=true"));
        assert!(result.contains("requires_approval=true"));
        // Verify it starts with \n\n
        assert!(result.starts_with("\n\n"));
    }

    // -- D11-021: mcp_extras format aligned with Python ---------------------

    #[test]
    fn test_mcp_extras_stripped_key_and_colon_format() {
        // [D11-021] mcp_ prefix must be stripped, separator is ": " not "=",
        // each extra is its own section outside [Annotations: ...].
        let mut extra = std::collections::HashMap::new();
        extra.insert("mcp_cost_usd".to_string(), serde_json::json!("0.01"));
        extra.insert("mcp_model".to_string(), serde_json::json!("gpt-4"));
        let ann = ModuleAnnotations {
            extra,
            ..Default::default()
        };
        let result = AnnotationMapper::to_description_suffix(Some(&ann));
        assert!(
            result.contains("cost_usd: 0.01"),
            "must strip mcp_ prefix and use ': ' separator; got: {result:?}"
        );
        assert!(
            result.contains("model: gpt-4"),
            "must strip mcp_ prefix and use ': ' separator; got: {result:?}"
        );
        // Must NOT be inside [Annotations: ...]
        // Check that the extras appear AFTER any annotation block or standalone
        assert!(
            !result.contains("mcp_cost_usd"),
            "mcp_ prefix must be stripped; got: {result:?}"
        );
        assert!(
            !result.contains("mcp_model"),
            "mcp_ prefix must be stripped; got: {result:?}"
        );
        // Each extra must be a separate section (joined by \n\n not \n)
        let cost_pos = result.find("cost_usd:").unwrap();
        let model_pos = result.find("model:").unwrap();
        let between = &result[cost_pos.min(model_pos)..cost_pos.max(model_pos)];
        assert!(
            between.contains("\n\n"),
            "each mcp extra must be a separate section joined by \\n\\n; got: {result:?}"
        );
    }

    #[test]
    fn test_mcp_extras_not_inside_annotations_block() {
        // [D11-021] mcp_ extras must be OUTSIDE [Annotations: ...] block.
        let mut extra = std::collections::HashMap::new();
        extra.insert("mcp_hint".to_string(), serde_json::json!("use sparingly"));
        let ann = ModuleAnnotations {
            readonly: true, // triggers [Annotations:] block
            extra,
            ..Default::default()
        };
        let result = AnnotationMapper::to_description_suffix(Some(&ann));
        // [Annotations:] block must be present for readonly
        assert!(result.contains("[Annotations:"));
        // hint: use sparingly must NOT be inside the [Annotations: ...] brackets
        if let (Some(ann_start), Some(ann_end)) = (result.find("[Annotations:"), result.find(']')) {
            let ann_block = &result[ann_start..=ann_end];
            assert!(
                !ann_block.contains("hint:"),
                "mcp_ extras must be outside [Annotations: ...]; got block: {ann_block:?}"
            );
        }
        assert!(
            result.contains("hint: use sparingly"),
            "stripped extra must appear in result; got: {result:?}"
        );
    }

    /// [A-D-AM-1] Only string-valued `mcp_` extras are passed through. Python
    /// guards with `isinstance(extra[key], str)` and TypeScript with
    /// `typeof value === "string"`; Rust used to filter on the key prefix alone
    /// and JSON-serialize numbers/booleans/objects into the LLM-facing
    /// description.
    #[test]
    fn test_mcp_extras_non_string_values_are_dropped() {
        let mut extra = std::collections::HashMap::new();
        extra.insert("mcp_count".to_string(), serde_json::json!(3));
        extra.insert("mcp_enabled".to_string(), serde_json::json!(true));
        extra.insert("mcp_meta".to_string(), serde_json::json!({"a": 1}));
        extra.insert("mcp_hint".to_string(), serde_json::json!("keep me"));
        let ann = ModuleAnnotations {
            extra,
            ..Default::default()
        };
        let result = AnnotationMapper::to_description_suffix(Some(&ann));
        assert!(
            result.contains("hint: keep me"),
            "string extras must survive; got: {result:?}"
        );
        for dropped in ["count", "enabled", "meta"] {
            assert!(
                !result.contains(dropped),
                "non-string extra {dropped:?} must be dropped; got: {result:?}"
            );
        }
    }

    /// A module whose ONLY extras are non-strings must produce no suffix at all.
    #[test]
    fn test_mcp_extras_only_non_string_yields_empty_suffix() {
        let mut extra = std::collections::HashMap::new();
        extra.insert("mcp_count".to_string(), serde_json::json!(3));
        let ann = ModuleAnnotations {
            extra,
            ..Default::default()
        };
        assert_eq!(AnnotationMapper::to_description_suffix(Some(&ann)), "");
    }

    // ---- has_requires_approval tests ----

    #[test]
    fn test_has_requires_approval_none() {
        assert!(!AnnotationMapper::has_requires_approval(None));
    }

    #[test]
    fn test_has_requires_approval_true() {
        let ann = ModuleAnnotations {
            requires_approval: true,
            ..Default::default()
        };
        assert!(AnnotationMapper::has_requires_approval(Some(&ann)));
    }

    #[test]
    fn test_has_requires_approval_false() {
        let ann = ModuleAnnotations::default();
        assert!(!AnnotationMapper::has_requires_approval(Some(&ann)));
    }
}
