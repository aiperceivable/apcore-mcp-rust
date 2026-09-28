//! Module-ID handling in the OpenAPI backend since apcore-toolkit 0.13.0.
//!
//! The toolkit now normalises every `module_id` it emits into apcore's
//! Canonical ID alphabet — after `base_path_prefix` and the ID-affecting hooks
//! — so the bridge registers the scanner's IDs as emitted and keeps only its
//! skip policy, applied to the ID `scan` returns. The shared fixture
//! (`openapi_backend.json`) pins the common cases in all three languages; these
//! tests add the Rust-reachable edges. Rust's `openapi_backend` exposes no
//! caller hooks, so the hook-returned-ID cases are covered by the Python and
//! TypeScript bridges only.

mod common;

use std::sync::Arc;

use apcore::Registry;
use apcore_mcp::openapi_backend::{is_legal_segment, openapi_backend, OpenAPIBackendOptions};
use serde_json::{json, Value};

fn ok() -> Value {
    json!({"responses": {"200": {"description": "ok"}}})
}

fn document(paths: Value) -> Value {
    json!({
        "openapi": "3.0.3",
        "info": {"title": "Petstore", "version": "1.0.0"},
        "servers": [{"url": "https://api.example.com"}],
        "paths": paths,
    })
}

fn registry_ids(registry: &Registry) -> Vec<String> {
    let mut ids = registry.list(None, None, Some(&["public", "hidden"]));
    ids.sort_unstable();
    ids
}

fn quiet_options() -> OpenAPIBackendOptions {
    let mut options = OpenAPIBackendOptions::new();
    options.acknowledge_unapproved_writes = true;
    options
}

#[tokio::test]
async fn registers_the_ids_the_scanner_emits() {
    // The bridge's old projection gave `listpets` and `pets.petid.get`.
    let mut list = ok();
    list["operationId"] = json!("listPets");
    let doc = document(json!({
        "/pets": {"get": list},
        "/pets/{petId}": {"get": ok()},
    }));
    let registry = openapi_backend(&doc, Arc::new(Registry::new()), quiet_options())
        .await
        .expect("builds");
    assert_eq!(registry_ids(&registry), ["list_pets", "pets.pet_id.get"]);
}

#[tokio::test]
async fn a_skip_names_the_emitted_id_after_deduplication() {
    // Two `operationId: 3ds` operations: normalisation cannot repair a leading
    // digit, and deduplication runs before the bridge sees the modules, so the
    // second skip warning must name `3ds_2`, the ID actually emitted.
    let mut first = ok();
    first["operationId"] = json!("3ds");
    let mut second = ok();
    second["operationId"] = json!("3ds");
    let mut list = ok();
    list["operationId"] = json!("listPets");
    let doc = document(json!({
        "/a": {"get": first},
        "/b": {"get": second},
        "/pets": {"get": list},
    }));

    let (logs, _guard) = common::capture_logs();
    let registry = openapi_backend(&doc, Arc::new(Registry::new()), quiet_options())
        .await
        .expect("builds");

    assert_eq!(registry_ids(&registry), ["list_pets"]);
    let skips: Vec<String> = logs
        .at(tracing::Level::WARN)
        .into_iter()
        .filter(|w| w.contains("OpenAPI operation skipped"))
        .collect();
    assert_eq!(skips.len(), 2, "got {skips:?}");
    assert!(skips[0].contains("'3ds'"), "got {skips:?}");
    assert!(skips[1].contains("'3ds_2'"), "got {skips:?}");
    assert!(
        logs.at(tracing::Level::ERROR).is_empty(),
        "a skipped module must not reach the writer: {:?}",
        logs.at(tracing::Level::ERROR)
    );
}

#[tokio::test]
async fn a_skipped_module_does_not_count_toward_the_collision_preflight() {
    let registry = Arc::new(Registry::new());
    common::register_stub(&registry, "keep");
    let mut list = ok();
    list["operationId"] = json!("listPets");
    let doc = document(json!({
        "/v1/2fa": {"post": ok()},
        "/pets": {"get": list},
    }));
    let registry = openapi_backend(&doc, registry, quiet_options())
        .await
        .expect("builds");
    assert_eq!(registry_ids(&registry), ["keep", "list_pets"]);
}

#[test]
#[allow(deprecated)]
fn project_module_id_is_deprecated_with_its_behaviour_unchanged() {
    use apcore_mcp::openapi_backend::project_module_id;

    assert_eq!(project_module_id("listPets").as_deref(), Some("listpets"));
    assert_eq!(
        project_module_id("pet-store.items.get").as_deref(),
        Some("pet_store.items.get")
    );
    assert_eq!(project_module_id("v1.2fa.post"), None);
    assert_eq!(project_module_id(""), None);
}

#[test]
fn is_legal_segment_is_the_registry_pattern_per_segment() {
    assert!(is_legal_segment("list_pets"));
    assert!(is_legal_segment("a1_"));
    assert!(!is_legal_segment("2fa"));
    assert!(!is_legal_segment("listPets"));
    assert!(!is_legal_segment("pet-store"));
    assert!(!is_legal_segment(""));
}
