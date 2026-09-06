//! MCP config namespace registration for the Config Bus (apcore 0.15.1 §9.4).
//!
//! Provides [`MCP_NAMESPACE`], [`MCP_ENV_PREFIX`], and [`register_mcp_namespace`]
//! for registering MCP-specific configuration with the apcore Config Bus.

use apcore::config::{Config, NamespaceRegistration};

/// Config Bus namespace name for apcore-mcp.
pub const MCP_NAMESPACE: &str = "mcp";

/// Environment variable prefix for the MCP namespace.
pub const MCP_ENV_PREFIX: &str = "APCORE_MCP";

/// Register the `mcp` config namespace with the apcore Config Bus.
///
/// Safe to call multiple times — ignores duplicate registration.
pub fn register_mcp_namespace() {
    let _ = Config::register_namespace(NamespaceRegistration {
        name: MCP_NAMESPACE.to_string(),
        env_prefix: Some(MCP_ENV_PREFIX.to_string()),
        defaults: Some(mcp_defaults()),
        schema: None,
        env_style: apcore::config::EnvStyle::Auto,
        max_depth: 4,
        env_map: None,
    });
}

/// Attempt to read the `mcp.pipeline` configuration from the Config Bus.
///
/// Returns `Some(Value)` if a "pipeline" key exists in the MCP namespace
/// configuration, `None` otherwise. This is used by F-040 (YAML Pipeline
/// Config) to load pipeline strategy from configuration files.
pub fn get_pipeline_config() -> Option<serde_json::Value> {
    // Discover the config (from file or defaults), then read the MCP namespace.
    // Called once during build(), so the file-system discovery cost is acceptable.
    let config = Config::discover().ok()?;
    config
        .namespace(MCP_NAMESPACE)
        .get("pipeline")
        .cloned()
        .filter(|v| !v.is_null())
}

/// Attempt to read the `mcp.middleware` configuration from the Config Bus.
///
/// Returns `Some(Value)` (expected to be a JSON array) if a "middleware" key
/// exists in the MCP namespace configuration, `None` otherwise. Consumed by
/// `middleware_builder::build_middleware_from_config` during `build()`.
pub fn get_middleware_config() -> Option<serde_json::Value> {
    let config = Config::discover().ok()?;
    config
        .namespace(MCP_NAMESPACE)
        .get("middleware")
        .cloned()
        .filter(|v| !v.is_null() && v.as_array().is_some_and(|a| !a.is_empty()))
}

/// Attempt to read the `mcp.acl` configuration from the Config Bus.
///
/// Returns `Some(Value)` (expected to be a JSON object with `rules` and
/// optional `default_effect`) if an "acl" key exists and is non-null in the
/// MCP namespace configuration, `None` otherwise. Consumed by
/// `acl_builder::build_acl_from_config` during `build()`.
///
/// **Enabling `sys_modules.enabled` without also configuring `mcp.acl`
/// leaves the entire `system.*` management surface — health, usage,
/// manifest, and `system.control.*` (reconfigure, reload, toggle) —
/// reachable with no authorization check at all.** See the ACL rule
/// template in `acl_builder`'s module doc comment
/// (aiperceivable/apcore-mcp#14) for the reference `system.*` rules, and
/// [`crate::apcore_mcp::APCoreMCP::serve`] for the startup warning this
/// bridge logs when the control surface is registered but unprotected.
pub fn get_acl_config() -> Option<serde_json::Value> {
    let config = Config::discover().ok()?;
    config
        .namespace(MCP_NAMESPACE)
        .get("acl")
        .cloned()
        .filter(|v| !v.is_null())
}

/// Read the `mcp.openapi` Config Bus section, or `None` when absent.
///
/// PRD F-054 Acceptance Criterion 1: `mcp.openapi.spec` alone, with no CLI
/// flag and no explicit `openapi_backend` call, must start a server. Before
/// this existed, `mcp.openapi` was published in [`mcp_defaults`] as a round-
/// tripping key but nothing ever read it back.
#[must_use]
pub fn get_openapi_config() -> Option<serde_json::Value> {
    let config = Config::discover().ok()?;
    config
        .namespace(MCP_NAMESPACE)
        .get("openapi")
        .cloned()
        .filter(|v| !v.is_null())
}

/// Scalar Config Bus values consumed by the convenience [`crate::serve`] /
/// [`crate::async_serve`] functions. Mirrors the 9 scalar keys declared in
/// [`mcp_defaults`] and the corresponding TypeScript `ConfigBusDefaults` so
/// callers setting `APCORE_MCP_PORT=9000` actually see the change. [D9-003]
#[derive(Debug, Default, Clone)]
pub struct McpScalarConfig {
    pub transport: Option<String>,
    pub host: Option<String>,
    pub port: Option<u16>,
    pub name: Option<String>,
    pub log_level: Option<String>,
    pub validate_inputs: Option<bool>,
    pub explorer: Option<bool>,
    pub explorer_prefix: Option<String>,
    pub require_auth: Option<bool>,
}

/// Read the 9 scalar `mcp.*` keys from the Config Bus.
///
/// Discovers the config exactly once and returns the keys typed for direct
/// use by the convenience entrypoints. Returns an all-None struct on any
/// error (Config Bus unavailable, namespace missing) so callers can fall
/// through to their hardcoded defaults.
pub fn get_scalar_config() -> McpScalarConfig {
    fn read(config: &Config, key: &str) -> Option<serde_json::Value> {
        config
            .namespace(MCP_NAMESPACE)
            .get(key)
            .cloned()
            .filter(|v| !v.is_null())
    }
    let Ok(config) = Config::discover() else {
        return McpScalarConfig::default();
    };
    McpScalarConfig {
        transport: read(&config, "transport").and_then(|v| v.as_str().map(str::to_string)),
        host: read(&config, "host").and_then(|v| v.as_str().map(str::to_string)),
        port: read(&config, "port").and_then(|v| {
            v.as_u64()
                .and_then(|n| u16::try_from(n).ok())
                .or_else(|| v.as_str().and_then(|s| s.parse::<u16>().ok()))
        }),
        name: read(&config, "name").and_then(|v| v.as_str().map(str::to_string)),
        log_level: read(&config, "log_level").and_then(|v| v.as_str().map(str::to_string)),
        validate_inputs: read(&config, "validate_inputs").and_then(|v| {
            v.as_bool().or_else(|| match v.as_str() {
                Some("true") => Some(true),
                Some("false") => Some(false),
                _ => None,
            })
        }),
        explorer: read(&config, "explorer").and_then(|v| {
            v.as_bool().or_else(|| match v.as_str() {
                Some("true") => Some(true),
                Some("false") => Some(false),
                _ => None,
            })
        }),
        explorer_prefix: read(&config, "explorer_prefix")
            .and_then(|v| v.as_str().map(str::to_string)),
        require_auth: read(&config, "require_auth").and_then(|v| {
            v.as_bool().or_else(|| match v.as_str() {
                Some("true") => Some(true),
                Some("false") => Some(false),
                _ => None,
            })
        }),
    }
}

/// Returns the default configuration values for the MCP namespace.
///
/// Mirrors `apcore_mcp.MCP_DEFAULTS` in the Python SDK and
/// `MCP_DEFAULTS` in the TypeScript SDK so all three language
/// bindings expose the same top-level defaults surface.
pub fn mcp_defaults() -> serde_json::Value {
    serde_json::json!({
        "transport": "stdio",
        "host": "127.0.0.1",
        "port": 8000,
        "name": "apcore-mcp",
        "log_level": null,
        "validate_inputs": false,
        "explorer": false,
        "explorer_prefix": "/explorer",
        "require_auth": true,
        // [A-013] No `output_format` key. Python's MCP_DEFAULTS (config.py:14)
        // and TypeScript's (config.ts:12) do not publish one, so honouring
        // `mcp.output_format` / `APCORE_MCP_OUTPUT_FORMAT` here made the same
        // Config Bus file produce CSV from the Rust bridge and JSON from the
        // other two. The option remains available programmatically
        // (`APCoreMCPBuilder::output_format`, `ServeConfig::output_format`) and
        // via the CLI's `--output-format`, as it is in all three SDKs.
        // Declarative middleware list. Each entry is { type: string, ...kwargs }.
        // See `middleware_builder::build_middleware_from_config` for supported types.
        "middleware": [],
        // Declarative ACL — { default_effect: "deny"|"allow", rules: [ACLRule...] }.
        // `null` or missing means "no ACL" (allow all). See `acl_builder::build_acl_from_config`.
        "acl": null,
        // OpenAPI backend — { spec, base_url, prefix, include, exclude,
        // include_deprecated, timeout, headers, acknowledge_unapproved_writes }.
        // `spec` is the FIRST path-typed key in this namespace and is
        // explicitly NOT covered by apcore 0.30.0's `Config::path_typed_keys()`
        // — see `openapi_backend::resolve_spec_location`, which owns its own
        // empty/URL/relative-path rules instead.
        "openapi": null
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_mcp_namespace_constant() {
        assert_eq!(MCP_NAMESPACE, "mcp");
    }

    #[test]
    fn test_mcp_env_prefix_constant() {
        assert_eq!(MCP_ENV_PREFIX, "APCORE_MCP");
    }

    /// [A-013] The Config Bus defaults surface must match Python's
    /// `MCP_DEFAULTS` (config.py:14) and TypeScript's `MCP_DEFAULTS`
    /// (config.ts:12) key for key. Rust used to publish a 12th key,
    /// `output_format`, so the same Config Bus file produced CSV output from
    /// the Rust bridge and JSON from the other two.
    #[test]
    fn test_mcp_defaults_key_set_matches_peer_sdks() {
        const CROSS_SDK_KEYS: &[&str] = &[
            "transport",
            "host",
            "port",
            "name",
            "log_level",
            "validate_inputs",
            "explorer",
            "explorer_prefix",
            "require_auth",
            "middleware",
            "acl",
            "openapi",
        ];

        let defaults = mcp_defaults();
        let obj = defaults.as_object().expect("defaults must be an object");
        let mut keys: Vec<&str> = obj.keys().map(String::as_str).collect();
        keys.sort_unstable();
        let mut expected: Vec<&str> = CROSS_SDK_KEYS.to_vec();
        expected.sort_unstable();
        assert_eq!(
            keys, expected,
            "the Config Bus defaults surface must match Python and TypeScript exactly"
        );
    }

    #[test]
    fn test_mcp_defaults_has_expected_keys() {
        let defaults = mcp_defaults();
        assert_eq!(defaults["transport"], "stdio");
        assert_eq!(defaults["host"], "127.0.0.1");
        assert_eq!(defaults["port"], 8000);
        assert_eq!(defaults["name"], "apcore-mcp");
        assert!(defaults["log_level"].is_null());
        assert_eq!(defaults["validate_inputs"], false);
        assert_eq!(defaults["explorer"], false);
        assert_eq!(defaults["explorer_prefix"], "/explorer");
        assert_eq!(defaults["require_auth"], true);
    }

    #[test]
    fn test_register_mcp_namespace_idempotent() {
        register_mcp_namespace();
        register_mcp_namespace(); // Should not panic
    }

    #[test]
    fn test_get_scalar_config_returns_struct_without_panic() {
        // [D9-003] get_scalar_config must not panic when no config file is
        // discoverable; it should return McpScalarConfig::default() (all None)
        // so callers fall through to their hardcoded defaults.
        let _scalar = get_scalar_config();
    }

    #[test]
    fn test_mcp_scalar_config_default_all_none() {
        // [D9-003] The default state means "no Config Bus override" — all
        // fields are None so the caller's ServeConfig values are preserved.
        let scalar = McpScalarConfig::default();
        assert!(scalar.transport.is_none());
        assert!(scalar.host.is_none());
        assert!(scalar.port.is_none());
        assert!(scalar.name.is_none());
        assert!(scalar.log_level.is_none());
        assert!(scalar.validate_inputs.is_none());
        assert!(scalar.explorer.is_none());
        assert!(scalar.explorer_prefix.is_none());
        assert!(scalar.require_auth.is_none());
    }
}
