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
    // [A-013] No `output_format` key — Python and TypeScript publish none.
    assert!(defaults.get("output_format").is_none());
    assert!(defaults["middleware"].is_array());
    assert!(defaults["acl"].is_null());
}

#[test]
fn meta_tool_names_is_exported_at_crate_root() {
    // [D1-002] Python and TypeScript SDKs expose `META_TOOL_NAMES` as a
    // top-level constant listing all five apcore meta-tool identifiers.
    use apcore_mcp::META_TOOL_NAMES;

    assert_eq!(META_TOOL_NAMES.len(), 5);
    // Every entry should be a meta-tool name carrying the apcore meta
    // prefix.
    for name in META_TOOL_NAMES.iter() {
        assert!(
            name.starts_with(apcore_mcp::APCORE_META_TOOL_PREFIX),
            "expected {} to begin with {}",
            name,
            apcore_mcp::APCORE_META_TOOL_PREFIX,
        );
    }
}

#[test]
fn async_task_bridge_is_exported_at_crate_root() {
    // [D1-002] Python and TypeScript SDKs export `AsyncTaskBridge` as a
    // top-level symbol. Verify the type name resolves through the Rust
    // crate root. We only need the type to be reachable — constructing
    // a real bridge is covered by the server-level tests because the
    // constructor takes live apcore executor handles.
    fn _assert_type<T>() {}
    _assert_type::<apcore_mcp::AsyncTaskBridge>();
}

#[test]
fn output_format_is_exported_at_crate_root() {
    // [B-RS-9] `APCoreMCPBuilder::output_format` takes
    // `server::router::OutputFormat`, so the documented call in README.md
    // cannot be written without a crate-root re-export — every other type in a
    // public builder signature has one.
    use apcore_mcp::OutputFormat;
    use std::str::FromStr;

    assert_eq!(OutputFormat::default(), OutputFormat::Json);
    assert_eq!(OutputFormat::from_str("csv").unwrap(), OutputFormat::Csv);
    assert_eq!(
        OutputFormat::from_str("jsonl").unwrap(),
        OutputFormat::Jsonl
    );
    assert!(OutputFormat::from_str("yaml").is_err());

    // FromStr and Display must round-trip: the type travels through config.
    assert_eq!(OutputFormat::Csv.to_string(), "csv");
    assert_eq!(
        OutputFormat::from_str(&OutputFormat::Jsonl.to_string()).unwrap(),
        OutputFormat::Jsonl
    );
}
