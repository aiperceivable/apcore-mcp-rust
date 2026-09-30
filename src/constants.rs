//! Constants used throughout the apcore-mcp bridge.
//!
//! Provides [`ErrorCode`], [`RegistryEvent`], and module ID validation.

use std::sync::LazyLock;

use regex::Regex;
use serde::{Deserialize, Serialize};
use strum_macros::{Display, EnumIter, EnumString, IntoStaticStr};

/// Standard error codes emitted by the apcore MCP bridge.
///
/// Each variant serializes to its SCREAMING_SNAKE_CASE string form
/// for wire-format compatibility with other language SDKs.
#[derive(
    Debug,
    Clone,
    Copy,
    PartialEq,
    Eq,
    Hash,
    Display,
    EnumString,
    EnumIter,
    IntoStaticStr,
    Serialize,
    Deserialize,
)]
#[strum(serialize_all = "SCREAMING_SNAKE_CASE")]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
#[non_exhaustive]
pub enum ErrorCode {
    ModuleNotFound,
    ModuleDisabled,
    SchemaValidationError,
    AclDenied,
    CallDepthExceeded,
    CircularCall,
    CallFrequencyExceeded,
    InternalError,
    GeneralInternalError,
    ModuleTimeout,
    ModuleLoadError,
    ModuleExecuteError,
    GeneralInvalidInput,
    ApprovalDenied,
    ApprovalTimeout,
    ApprovalPending,
    VersionIncompatible,
    ErrorCodeCollision,
    ExecutionCancelled,
    ConfigNamespaceDuplicate,
    ConfigNamespaceReserved,
    ConfigEnvPrefixConflict,
    ConfigMountError,
    ConfigBindError,
    ErrorFormatterDuplicate,
    ConfigEnvMapConflict,
    PipelineAbort,
    StepNotFound,
    // New error codes introduced by apcore 0.19.0 (protocol-spec §5.12.8
    // and PROTOCOL_SPEC §5.3 / §5.15.2). Surfaced here for cross-SDK parity.
    DependencyNotFound,
    DependencyVersionMismatch,
    TaskLimitExceeded,
    VersionConstraintInvalid,
    BindingSchemaInferenceFailed,
    BindingSchemaModeConflict,
    BindingStrictSchemaIncompatible,
    BindingPolicyViolation,
    // apcore 0.20.0 sync alignment A-001: CircuitBreakerMiddleware
    // emits CIRCUIT_BREAKER_OPEN when the breaker is in the OPEN state.
    // Cross-SDK parity with Python+TS ERROR_CODES.
    CircuitBreakerOpen,
}

/// Registry lifecycle events.
///
/// The wire value is lowercase (`"register"`, `"unregister"`).
/// Use [`RegistryEvent::key()`] for the uppercase protocol key.
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, Hash, Display, EnumString, EnumIter, Serialize, Deserialize,
)]
#[strum(serialize_all = "lowercase")]
#[serde(rename_all = "lowercase")]
pub enum RegistryEvent {
    Register,
    Unregister,
}

impl RegistryEvent {
    /// Returns the uppercase protocol key (e.g., `"REGISTER"`).
    pub const fn key(&self) -> &'static str {
        match self {
            Self::Register => "REGISTER",
            Self::Unregister => "UNREGISTER",
        }
    }
}

/// Dot-namespaced event types introduced in apcore 0.15.0 (§9.16).
///
/// These constants provide canonical event type names for the apcore event system.
/// The `RegistryListener` uses callback-based `registry.on("register")` which is
/// unaffected by these names — they are for consumer use when subscribing to the
/// apcore `EventEmitter`.
pub mod apcore_events {
    /// Module toggled on/off (replaces legacy `"module_health_changed"` for toggles).
    pub const MODULE_TOGGLED: &str = "apcore.module.toggled";
    /// Module hot-reloaded (replaces legacy `"config_changed"` for reloads).
    pub const MODULE_RELOADED: &str = "apcore.module.reloaded";
    /// Runtime config key updated (replaces legacy `"config_changed"` for updates).
    pub const CONFIG_UPDATED: &str = "apcore.config.updated";
    /// Error rate recovered below threshold.
    pub const HEALTH_RECOVERED: &str = "apcore.health.recovered";
}

/// Regex pattern for valid module IDs.
///
/// Module IDs are lowercase dot-separated segments: `core`, `image.resize`,
/// `core.utils.string_ops`. Each segment starts with a letter and may contain
/// lowercase letters, digits, and underscores.
///
/// Matches the Python reference: `^[a-z][a-z0-9_]*(\.[a-z][a-z0-9_]*)*$`
pub const MODULE_ID_PATTERN: &str = r"^[a-z][a-z0-9_]*(\.[a-z][a-z0-9_]*)*$";

/// Returns a compiled [`Regex`] for [`MODULE_ID_PATTERN`].
///
/// The regex is compiled once and cached for the lifetime of the process.
pub fn module_id_regex() -> &'static Regex {
    static RE: LazyLock<Regex> =
        LazyLock::new(|| Regex::new(MODULE_ID_PATTERN).expect("MODULE_ID_PATTERN is valid regex"));
    &RE
}

/// Returns `true` if `id` is a valid module identifier.
///
/// This is a convenience wrapper around [`module_id_regex()`].
pub fn is_valid_module_id(id: &str) -> bool {
    module_id_regex().is_match(id)
}

#[cfg(test)]
mod tests {
    use super::*;
    use strum::IntoEnumIterator;

    #[test]
    fn error_code_display_round_trip() {
        for code in ErrorCode::iter() {
            let s = code.to_string();
            let parsed: ErrorCode = s.parse().unwrap();
            assert_eq!(parsed, code);
        }
    }

    #[test]
    fn error_code_serde_round_trip() {
        for code in ErrorCode::iter() {
            let json = serde_json::to_string(&code).unwrap();
            let parsed: ErrorCode = serde_json::from_str(&json).unwrap();
            assert_eq!(parsed, code);
        }
    }

    #[test]
    fn error_code_count() {
        // Intentional guard: update this count when adding new error codes
        // to ensure the strum EnumIter derivation stays in sync.
        assert_eq!(ErrorCode::iter().count(), 37);
    }

    #[test]
    fn error_code_known_values() {
        assert_eq!(ErrorCode::ModuleNotFound.to_string(), "MODULE_NOT_FOUND");
        assert_eq!(
            ErrorCode::SchemaValidationError.to_string(),
            "SCHEMA_VALIDATION_ERROR"
        );
        assert_eq!(
            ErrorCode::ExecutionCancelled.to_string(),
            "EXECUTION_CANCELLED"
        );
    }

    #[test]
    fn error_code_from_str_invalid() {
        assert!("NOT_A_REAL_CODE".parse::<ErrorCode>().is_err());
    }

    #[test]
    fn registry_event_display() {
        assert_eq!(RegistryEvent::Register.to_string(), "register");
        assert_eq!(RegistryEvent::Unregister.to_string(), "unregister");
    }

    #[test]
    fn registry_event_from_str() {
        assert_eq!(
            "register".parse::<RegistryEvent>().unwrap(),
            RegistryEvent::Register
        );
        assert_eq!(
            "unregister".parse::<RegistryEvent>().unwrap(),
            RegistryEvent::Unregister
        );
    }

    #[test]
    fn registry_event_serde_round_trip() {
        for event in RegistryEvent::iter() {
            let json = serde_json::to_string(&event).unwrap();
            let parsed: RegistryEvent = serde_json::from_str(&json).unwrap();
            assert_eq!(parsed, event);
        }
    }

    #[test]
    fn registry_event_key() {
        assert_eq!(RegistryEvent::Register.key(), "REGISTER");
        assert_eq!(RegistryEvent::Unregister.key(), "UNREGISTER");
    }

    #[test]
    fn module_id_pattern_valid() {
        let re = module_id_regex();
        assert!(re.is_match("image.resize"));
        assert!(re.is_match("core.utils.string_ops"));
        assert!(re.is_match("core"));
        assert!(re.is_match("a"));
        assert!(re.is_match("my_module.v2_helper"));
    }

    #[test]
    fn module_id_pattern_invalid() {
        let re = module_id_regex();
        assert!(!re.is_match(""));
        assert!(!re.is_match("Image.Resize"));
        assert!(!re.is_match("2fast.module"));
        assert!(!re.is_match("my-module.resize"));
        assert!(!re.is_match(".leading.dot"));
        assert!(!re.is_match("trailing.dot."));
        assert!(!re.is_match("double..dot"));
        assert!(!re.is_match("has space.mod"));
    }

    #[test]
    fn module_id_pattern_string() {
        assert!(MODULE_ID_PATTERN.starts_with('^'));
        assert!(MODULE_ID_PATTERN.ends_with('$'));
    }

    #[test]
    fn is_valid_module_id_delegates() {
        assert!(is_valid_module_id("core"));
        assert!(is_valid_module_id("image.resize"));
        assert!(!is_valid_module_id(""));
        assert!(!is_valid_module_id("UPPER"));
    }

    // ---- Integration tests ----

    /// Verify every Python ERROR_CODES key can be parsed into an ErrorCode variant.
    #[test]
    fn all_python_error_codes_parse() {
        let python_codes = [
            "MODULE_NOT_FOUND",
            "MODULE_DISABLED",
            "SCHEMA_VALIDATION_ERROR",
            "ACL_DENIED",
            "CALL_DEPTH_EXCEEDED",
            "CIRCULAR_CALL",
            "CALL_FREQUENCY_EXCEEDED",
            "INTERNAL_ERROR",
            "GENERAL_INTERNAL_ERROR",
            "MODULE_TIMEOUT",
            "MODULE_LOAD_ERROR",
            "MODULE_EXECUTE_ERROR",
            "GENERAL_INVALID_INPUT",
            "APPROVAL_DENIED",
            "APPROVAL_TIMEOUT",
            "APPROVAL_PENDING",
            "VERSION_INCOMPATIBLE",
            "ERROR_CODE_COLLISION",
            "EXECUTION_CANCELLED",
            "CONFIG_NAMESPACE_DUPLICATE",
            "CONFIG_NAMESPACE_RESERVED",
            "CONFIG_ENV_PREFIX_CONFLICT",
            "CONFIG_MOUNT_ERROR",
            "CONFIG_BIND_ERROR",
            "ERROR_FORMATTER_DUPLICATE",
            "CONFIG_ENV_MAP_CONFLICT",
            "PIPELINE_ABORT",
            "STEP_NOT_FOUND",
            "DEPENDENCY_NOT_FOUND",
            "DEPENDENCY_VERSION_MISMATCH",
            "TASK_LIMIT_EXCEEDED",
            "VERSION_CONSTRAINT_INVALID",
            "BINDING_SCHEMA_INFERENCE_FAILED",
            "BINDING_SCHEMA_MODE_CONFLICT",
            "BINDING_STRICT_SCHEMA_INCOMPATIBLE",
            "BINDING_POLICY_VIOLATION",
            "CIRCUIT_BREAKER_OPEN",
        ];
        for code_str in &python_codes {
            let parsed: ErrorCode = code_str
                .parse()
                .unwrap_or_else(|_| panic!("Failed to parse Python error code: {code_str}"));
            assert_eq!(&parsed.to_string(), *code_str);
        }
        assert_eq!(python_codes.len(), ErrorCode::iter().count());
    }

    /// Verify RegistryEvent wire values match Python REGISTRY_EVENTS dict values.
    #[test]
    fn registry_events_match_python() {
        assert_eq!(RegistryEvent::Register.to_string(), "register");
        assert_eq!(RegistryEvent::Unregister.to_string(), "unregister");
        assert_eq!(RegistryEvent::Register.key(), "REGISTER");
        assert_eq!(RegistryEvent::Unregister.key(), "UNREGISTER");
    }

    /// Verify MODULE_ID_PATTERN matches the Python regex exactly.
    #[test]
    fn module_id_pattern_matches_python() {
        assert_eq!(MODULE_ID_PATTERN, r"^[a-z][a-z0-9_]*(\.[a-z][a-z0-9_]*)*$");
    }

    /// ErrorCode JSON output is a plain string, not an object.
    #[test]
    fn error_code_json_is_plain_string() {
        let json = serde_json::to_value(ErrorCode::InternalError).unwrap();
        assert!(json.is_string());
        assert_eq!(json.as_str().unwrap(), "INTERNAL_ERROR");
    }
}
