//! Markdown rendering for apcore modules via apcore-toolkit.
//!
//! LLMs read MCP/OpenAI tool `description` strings as their primary
//! signal for tool selection — the richer the description, the better
//! the agent picks the right tool. apcore-toolkit's
//! `format_module(style = Markdown)` emits a canonical, cross-SDK
//! byte-equivalent rendering with title, description, parameters,
//! returns, behavior table, tags, and examples.
//!
//! This module bridges apcore's `ModuleDescriptor` (the runtime type
//! flowing through apcore-mcp) to apcore-toolkit's `ScannedModule`
//! (the input format `format_module` expects), then delegates.

use std::collections::HashMap;

use apcore::registry::ModuleDescriptor;
use apcore_toolkit::{format_module, FormatOutput, ModuleStyle, ScannedModule};

/// Adapt an apcore [`ModuleDescriptor`] to a toolkit [`ScannedModule`].
///
/// The two types are near-supersets of each other — overlapping fields
/// are copied verbatim and toolkit-only fields (`target`,
/// `documentation`, `suggested_alias`, `warnings`) get sensible
/// defaults so `format_module` produces identical output regardless of
/// which type the caller starts from.
pub fn descriptor_to_scanned_module(descriptor: &ModuleDescriptor) -> ScannedModule {
    ScannedModule {
        module_id: descriptor.module_id.clone(),
        description: descriptor.description.clone(),
        input_schema: descriptor.input_schema.clone(),
        output_schema: descriptor.output_schema.clone(),
        tags: descriptor.tags.clone(),
        // ModuleDescriptor doesn't carry a callable target string; emit
        // an empty placeholder. format_module(markdown) doesn't render
        // `target` so this has no observable effect on the output.
        target: String::new(),
        version: descriptor.version.clone(),
        annotations: descriptor.annotations.clone(),
        documentation: descriptor.documentation.clone(),
        suggested_alias: None,
        examples: descriptor.examples.clone(),
        // Convert metadata: ModuleDescriptor uses HashMap<String, Value>,
        // ScannedModule does too — same shape, direct clone.
        metadata: descriptor
            .metadata
            .iter()
            .map(|(k, v)| (k.clone(), v.clone()))
            .collect::<HashMap<_, _>>(),
        display: descriptor.display.clone(),
        warnings: vec![],
    }
}

/// Whether apcore-toolkit's Markdown renderer is usable in this process.
///
/// Named by the feature Contract and PRD F-049 AC2 alongside Python's
/// `is_available()` and TypeScript's `isMarkdownAvailable()`.
///
/// Rust caveat: `lib.rs` declares `pub mod markdown;` unconditionally and the
/// crate has no `[features]` section, so apcore-toolkit is a compile-time
/// dependency — if this crate links, the renderer is present. The Contract's
/// "toolkit unavailable" row therefore cannot arise here, and this honestly
/// returns `true` rather than pretending to probe.
pub fn is_available() -> bool {
    true
}

/// Pre-load the Markdown renderer. No-op in Rust; exists for API parity.
///
/// TypeScript needs this to settle a dynamic `import()` before synchronous
/// render calls. Rust links the toolkit statically, so there is nothing to
/// prime — the Contract states plainly that Python and Rust expose it as a
/// no-op.
pub fn prime_markdown_toolkit() {}

/// Render a [`ModuleDescriptor`] as canonical apcore-toolkit Markdown.
///
/// Returns the Markdown body produced by
/// `format_module(scanned, ModuleStyle::Markdown, display)` — title,
/// description, parameters list, returns list, behavior table (toolkit
/// 0.6.0 emits only fields differing from defaults), tags, and
/// examples.
///
/// Returns `None` when the toolkit yields a non-text variant. The Contract
/// pins `Option<String>` with None on render failure; previously this
/// substituted `descriptor.description` on that arm, which made a failed
/// render indistinguishable from a successful render of a plain-text module.
/// Callers own the fallback.
pub fn render_module_markdown(descriptor: &ModuleDescriptor, display: bool) -> Option<String> {
    let scanned = descriptor_to_scanned_module(descriptor);
    match format_module(&scanned, ModuleStyle::Markdown, display) {
        FormatOutput::Text(text) => Some(text),
        other => {
            tracing::warn!(
                "apcore-toolkit returned a non-text variant for Markdown style \
                 (module {}): {other:?} — falling back to the plain description",
                descriptor.module_id
            );
            None
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// [A-D-MD-1] The Contract names `is_available()` explicitly and PRD F-049
    /// AC2 repeats it. It was missing entirely from the Rust module.
    #[test]
    fn is_available_reports_toolkit_presence() {
        assert!(is_available());
    }

    /// [A-D-MD-2] The Contract states Python and Rust expose
    /// `prime_markdown_toolkit` for API parity as a no-op. It must exist and
    /// be safe to call repeatedly.
    #[test]
    fn prime_markdown_toolkit_is_an_idempotent_no_op() {
        prime_markdown_toolkit();
        prime_markdown_toolkit();
    }

    /// [A-D-MD-3] The Contract pins `Option<String>` with None on render
    /// failure. Returning a bare `String` that silently substituted
    /// `descriptor.description` made a failed render indistinguishable from a
    /// successful render of a plain-text module.
    #[test]
    fn render_module_markdown_returns_option() {
        let descriptor = ModuleDescriptor {
            module_id: "demo.echo".to_string(),
            name: None,
            description: "Echo the input".to_string(),
            documentation: None,
            input_schema: serde_json::json!({"type": "object", "properties": {}}),
            output_schema: serde_json::json!({}),
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
        let rendered: Option<String> = render_module_markdown(&descriptor, true);
        let rendered = rendered.expect("toolkit is linked in, so Markdown must render");
        assert!(
            rendered.contains("demo.echo") || rendered.contains("Echo the input"),
            "expected the module rendered into the Markdown body; got: {rendered}"
        );
    }
}
