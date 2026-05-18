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
    let ns_value = config.namespace(MCP_NAMESPACE)?;
    ns_value.get("pipeline").cloned().filter(|v| !v.is_null())
}

/// Attempt to read the `mcp.middleware` configuration from the Config Bus.
///
/// Returns `Some(Value)` (expected to be a JSON array) if a "middleware" key
/// exists in the MCP namespace configuration, `None` otherwise. Consumed by
/// `middleware_builder::build_middleware_from_config` during `build()`.
pub fn get_middleware_config() -> Option<serde_json::Value> {
    let config = Config::discover().ok()?;
    let ns_value = config.namespace(MCP_NAMESPACE)?;
    ns_value
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
pub fn get_acl_config() -> Option<serde_json::Value> {
    let config = Config::discover().ok()?;
    let ns_value = config.namespace(MCP_NAMESPACE)?;
    ns_value.get("acl").cloned().filter(|v| !v.is_null())
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
    pub output_format: Option<crate::server::router::OutputFormat>,
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
            .and_then(|ns| ns.get(key).cloned())
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
        output_format: read(&config, "output_format")
            .and_then(|v| v.as_str().and_then(|s| s.parse().ok())),
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
        "output_format": "json",
        // Declarative middleware list. Each entry is { type: string, ...kwargs }.
        // See `middleware_builder::build_middleware_from_config` for supported types.
        "middleware": [],
        // Declarative ACL — { default_effect: "deny"|"allow", rules: [ACLRule...] }.
        // `null` or missing means "no ACL" (allow all). See `acl_builder::build_acl_from_config`.
        "acl": null
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
