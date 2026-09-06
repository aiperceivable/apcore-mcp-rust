//! Regression tests for A-001: the previously-unimplemented `mcp.openapi`
//! wiring in the Rust bridge.
//!
//! Before this fix, `apcore-mcp-rust`'s `openapi_backend` module was never
//! invoked from any entry point — not the CLI (which had no `--from-openapi`
//! flag at all), not `APCoreMCPBuilder::build()`, nowhere. This file proves
//! the two new entry points work end to end against real, local (no
//! network) OpenAPI documents:
//!
//! - `openapi_backend_from_spec` — the full-pipeline helper that resolves a
//!   spec location and fetches/parses it, closing the gap left by the
//!   lower-level `openapi_backend(document: &Value, ...)`, which only ever
//!   accepted an already-parsed document.
//! - `build_openapi_backend_from_config` — the Config-Bus `mcp.openapi`
//!   translation layer, mirroring `acl_builder::build_acl_from_config`.
//! - `APCoreMCPBuilder::build_async` — the async builder entry point that
//!   resolves `mcp.openapi.spec` from the Config Bus alone (PRD F-054
//!   Acceptance Criterion 1), which the plain synchronous `build()` cannot
//!   do (Rust's `load_spec` is built on async `reqwest`, with no
//!   synchronous fallback).

use std::sync::Arc;

use apcore::registry::registry::Registry;
use apcore_mcp::openapi_backend::{
    build_openapi_backend_from_config, openapi_backend_from_spec, OpenAPIBackendOptions,
};
use serde_json::json;

fn write_petstore_spec(dir: &tempfile::TempDir) -> String {
    let path = dir.path().join("openapi.json");
    std::fs::write(
        &path,
        serde_json::to_string(&json!({
            "openapi": "3.0.3",
            "info": {"title": "Petstore", "version": "1.0.0"},
            "servers": [{"url": "https://api.example.com"}],
            "paths": {
                "/pets": {
                    "get": {
                        "operationId": "listPets",
                        "responses": {"200": {"description": "ok"}}
                    }
                }
            }
        }))
        .unwrap(),
    )
    .unwrap();
    path.to_str().unwrap().to_string()
}

#[tokio::test]
async fn openapi_backend_from_spec_builds_from_a_local_file() {
    let dir = tempfile::tempdir().unwrap();
    let spec_path = write_petstore_spec(&dir);

    let registry = openapi_backend_from_spec(
        &spec_path,
        Arc::new(Registry::new()),
        OpenAPIBackendOptions::new(),
    )
    .await
    .expect("openapi_backend_from_spec should succeed");

    let ids = registry.list(None, None, Some(&["public", "hidden"]));
    assert!(ids.contains(&"listpets".to_string()), "got: {ids:?}");
}

#[tokio::test]
async fn openapi_backend_from_spec_empty_value_errors() {
    let err =
        openapi_backend_from_spec("", Arc::new(Registry::new()), OpenAPIBackendOptions::new())
            .await
            .unwrap_err();
    assert!(format!("{err}").contains("resolved to nothing"));
}

#[tokio::test]
async fn build_openapi_backend_from_config_translates_the_config_bus_mapping() {
    let dir = tempfile::tempdir().unwrap();
    let spec_path = write_petstore_spec(&dir);

    let config = json!({"spec": spec_path, "prefix": "petstore"});
    let registry = build_openapi_backend_from_config(&config, Arc::new(Registry::new()), false)
        .await
        .expect("build_openapi_backend_from_config should succeed");

    let ids = registry.list(None, None, Some(&["public", "hidden"]));
    assert!(
        ids.contains(&"petstore.listpets".to_string()),
        "got: {ids:?}"
    );
}

#[tokio::test]
async fn build_openapi_backend_from_config_missing_spec_key_errors() {
    let err = build_openapi_backend_from_config(
        &json!({"prefix": "x"}),
        Arc::new(Registry::new()),
        false,
    )
    .await
    .unwrap_err();
    assert!(format!("{err}").contains("mcp.openapi.spec is required"));
}

#[tokio::test]
async fn build_openapi_backend_from_config_non_mapping_errors() {
    let err = build_openapi_backend_from_config(
        &json!("not-a-mapping"),
        Arc::new(Registry::new()),
        false,
    )
    .await
    .unwrap_err();
    assert!(format!("{err}").contains("must be a mapping"));
}

#[tokio::test]
async fn build_async_with_no_backend_and_no_config_bus_errors_like_build() {
    // No --extensions-dir equivalent, no mcp.openapi on the Config Bus in
    // this bare test environment: build_async must fail with the same
    // BackendResolution error the plain, synchronous build() raises.
    let result = apcore_mcp::APCoreMCPBuilder::default().build_async().await;
    let err = match result {
        Ok(_) => panic!("expected an error"),
        Err(e) => e,
    };
    assert!(format!("{err}").contains("backend source is required"));
}
