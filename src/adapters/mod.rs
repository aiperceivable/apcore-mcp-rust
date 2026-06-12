//! Adapters sub-module — translate between apcore and MCP data models.
//!
//! Provides shared error types and re-exports each adapter's public API.

pub mod annotations;
pub mod approval;
pub mod errors;
pub mod formatter;
pub mod id_normalizer;
pub mod schema;

// ---- Shared adapter error type ----------------------------------------------

use crate::constants::MODULE_ID_PATTERN;

/// Errors originating from the adapter layer.
///
/// Each variant captures enough context for callers to produce meaningful
/// diagnostics without leaking implementation details to the MCP client.
#[derive(Debug, thiserror::Error)]
pub enum AdapterError {
    /// A JSON Schema could not be converted between apcore and MCP formats.
    #[error("schema conversion failed: {0}")]
    SchemaConversion(String),

    /// A module ID did not match the expected pattern.
    #[error("invalid module ID '{id}': must match {pattern}")]
    InvalidModuleId { id: String, pattern: &'static str },
}

impl AdapterError {
    /// Convenience constructor for [`AdapterError::InvalidModuleId`] that
    /// automatically fills in the canonical [`MODULE_ID_PATTERN`].
    pub fn invalid_module_id(id: impl Into<String>) -> Self {
        Self::InvalidModuleId {
            id: id.into(),
            pattern: MODULE_ID_PATTERN,
        }
    }
}

// ---- Re-exports -------------------------------------------------------------

pub use annotations::AnnotationMapper;
pub use approval::ElicitationApprovalHandler;
pub use approval::StorageBackedApprovalHandler;
pub use errors::ErrorMapper;
pub use errors::{internal_error_response, register_mcp_formatter, McpErrorFormatter};
pub use id_normalizer::ModuleIDNormalizer;
pub use schema::SchemaConverter;

// ---- Tests ------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn adapter_error_schema_conversion_display() {
        let err = AdapterError::SchemaConversion("missing 'type' field".into());
        let msg = err.to_string();
        assert_eq!(msg, "schema conversion failed: missing 'type' field");
    }

    #[test]
    fn adapter_error_invalid_module_id_display() {
        let err = AdapterError::InvalidModuleId {
            id: "BAD-ID".into(),
            pattern: MODULE_ID_PATTERN,
        };
        let msg = err.to_string();
        assert!(msg.contains("BAD-ID"), "message should contain the ID");
        assert!(
            msg.contains(MODULE_ID_PATTERN),
            "message should contain the pattern"
        );
    }

    #[test]
    fn adapter_error_invalid_module_id_convenience() {
        let err = AdapterError::invalid_module_id("not.Valid");
        let msg = err.to_string();
        assert!(msg.contains("not.Valid"));
        assert!(msg.contains(MODULE_ID_PATTERN));
    }

    #[test]
    fn adapter_error_implements_std_error() {
        let err: Box<dyn std::error::Error> =
            Box::new(AdapterError::SchemaConversion("test".into()));
        // If this compiles, the trait is implemented.
        assert!(!err.to_string().is_empty());
    }
}
