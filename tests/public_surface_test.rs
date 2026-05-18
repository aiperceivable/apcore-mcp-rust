//! Public-surface regression tests for cross-SDK parity.
//!
//! These tests assert that symbols Python and TypeScript SDKs export at
//! their package top level are also reachable through the Rust crate
//! root. Regressions here usually indicate a missing re-export in
//! `src/lib.rs` rather than a behavior bug.

#[test]
fn mcp_defaults_is_exported_at_crate_root() {
    // [D1-001] `apcore_mcp::MCP_DEFAULTS` (Python) and
    // `MCP_DEFAULTS` (TypeScript) are top-level constants. Rust exposes
    // the same data through the `mcp_defaults()` function — verify the
    // re-export compiles and returns the expected shape.
    use apcore_mcp::mcp_defaults;

    let defaults = mcp_defaults();
    assert_eq!(defaults["transport"], "stdio");
    assert_eq!(defaults["host"], "127.0.0.1");
    assert_eq!(defaults["port"], 8000);
    assert_eq!(defaults["name"], "apcore-mcp");
    assert_eq!(defaults["validate_inputs"], false);
    assert_eq!(defaults["explorer"], false);
    assert_eq!(defaults["explorer_prefix"], "/explorer");
    assert_eq!(defaults["require_auth"], true);
    assert_eq!(defaults["output_format"], "json");
    assert!(defaults["middleware"].is_array());
    assert!(defaults["acl"].is_null());
}
