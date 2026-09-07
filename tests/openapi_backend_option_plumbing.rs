//! Option-plumbing regressions for the OpenAPI backend.
//!
//! Distinct from `openapi_backend_wiring.rs`, which covers A-001 — whether
//! `openapi_backend` is reached from the CLI and the Config Bus at all. This
//! file covers whether the OPTIONS survive that trip.
//!
//! `openapi_backend_conformance.rs` hands `openapi_backend` an already-parsed
//! document and calls `resolve_spec_location` directly with an explicit
//! `project_root`. That covers the pure functions and misses everything
//! between them — which is where three shipped defects lived:
//!
//!   - `--openapi-header` / `mcp.openapi.headers` built a header map that was
//!     never passed to the fetch (apcore-mcp-rust#8).
//!   - `mcp.openapi.timeout` configured the proxy timeout instead of the spec
//!     fetch, the opposite of the documented contract (apcore-mcp-rust#9).
//!   - `Config::project_root` was never read, so a relative `spec` resolved
//!     against the process CWD on every route (apcore-mcp#19).
//!
//! Each needs a real fetch or a real `Config`, so these tests run a local HTTP
//! server and a temporary project root rather than asserting on options.

use std::collections::HashMap;
use std::sync::{Arc, Mutex, OnceLock};

use apcore::{Context, Registry};
use apcore_mcp::openapi_backend::{
    build_openapi_backend_from_config, openapi_backend_from_spec, OpenAPIBackendOptions,
};
use serde_json::{json, Value};

/// `APCORE_CONFIG_FILE` is process-global, so the tests that set it take this
/// lock rather than racing each other inside the shared test binary.
fn env_lock() -> &'static tokio::sync::Mutex<()> {
    static LOCK: OnceLock<tokio::sync::Mutex<()>> = OnceLock::new();
    LOCK.get_or_init(|| tokio::sync::Mutex::new(()))
}

fn document() -> Value {
    json!({
        "openapi": "3.0.0",
        "info": { "title": "Petstore", "version": "1.0.0" },
        "servers": [{ "url": "https://api.example.com" }],
        "paths": {
            "/pets": {
                "get": {
                    "operationId": "listPets",
                    "responses": { "200": { "description": "ok" } }
                }
            }
        }
    })
}

/// Delay before the spec body is written.
const SERVE_DELAY: std::time::Duration = std::time::Duration::from_millis(120);

struct SpecServer {
    /// The spec document's URL.
    url: String,
    /// Origin for proxied calls — the same server answers `GET /pets`.
    base: String,
    seen_headers: Arc<Mutex<HashMap<String, String>>>,
}

type SeenHeaders = Arc<Mutex<HashMap<String, String>>>;

/// Record the request headers, then answer with `document()` after `SERVE_DELAY`.
async fn serve_spec(
    axum::extract::State(seen): axum::extract::State<SeenHeaders>,
    headers: axum::http::HeaderMap,
) -> axum::Json<Value> {
    {
        let mut recorded = seen.lock().unwrap();
        for (name, value) in &headers {
            if let Ok(v) = value.to_str() {
                recorded.insert(name.as_str().to_ascii_lowercase(), v.to_string());
            }
        }
    }
    tokio::time::sleep(SERVE_DELAY).await;
    axum::Json(document())
}

/// The proxied operation's target, answering on the same `SERVE_DELAY`.
async fn serve_pets() -> axum::Json<Value> {
    tokio::time::sleep(SERVE_DELAY).await;
    axum::Json(json!([{ "id": 1, "name": "Rex" }]))
}

/// Serve `document()` over HTTP after `SERVE_DELAY`, recording request headers.
async fn spawn_spec_server() -> SpecServer {
    let seen: SeenHeaders = Arc::new(Mutex::new(HashMap::new()));
    let app = axum::Router::new()
        .route("/openapi.json", axum::routing::get(serve_spec))
        .route("/pets", axum::routing::get(serve_pets))
        .with_state(Arc::clone(&seen));

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });

    SpecServer {
        url: format!("http://{addr}/openapi.json"),
        base: format!("http://{addr}"),
        seen_headers: seen,
    }
}

/// Module IDs in `registry`, including hidden ones.
fn registry_ids(registry: &Registry) -> Vec<String> {
    registry.list(None, None, Some(&["public", "hidden"]))
}

// ---------------------------------------------------------------------------
// #8 — headers reach the spec fetch
// ---------------------------------------------------------------------------

#[tokio::test]
async fn config_headers_reach_the_spec_fetch() {
    let server = spawn_spec_server().await;
    let registry = build_openapi_backend_from_config(
        &json!({
            "spec": server.url,
            "base_url": "https://api.example.com",
            "headers": { "X-Api-Key": "s3cret", "X-Tenant": "acme" },
        }),
        Arc::new(Registry::new()),
        false,
    )
    .await
    .expect("backend builds");

    assert!(registry_ids(&registry).contains(&"listpets".to_string()));

    let seen = server.seen_headers.lock().unwrap();
    assert_eq!(
        seen.get("x-api-key").map(String::as_str),
        Some("s3cret"),
        "mcp.openapi.headers must be sent with the spec fetch, saw {seen:?}"
    );
    assert_eq!(seen.get("x-tenant").map(String::as_str), Some("acme"));
}

#[tokio::test]
async fn options_headers_reach_the_spec_fetch() {
    let server = spawn_spec_server().await;
    let mut headers = HashMap::new();
    headers.insert("X-Api-Key".to_string(), "from-cli".to_string());

    let options = OpenAPIBackendOptions {
        base_url: Some("https://api.example.com".to_string()),
        headers: Some(headers),
        ..OpenAPIBackendOptions::new()
    };
    openapi_backend_from_spec(&server.url, Arc::new(Registry::new()), options)
        .await
        .expect("backend builds");

    let seen = server.seen_headers.lock().unwrap();
    assert_eq!(seen.get("x-api-key").map(String::as_str), Some("from-cli"));
}

#[tokio::test]
async fn a_non_string_header_value_is_a_startup_error() {
    // `X-Version: 1.0` in YAML is a number. Dropping it silently would
    // reproduce #8's exact symptom — a header configured and not sent — so it
    // fails at startup instead, naming the key and the fix.
    let err = build_openapi_backend_from_config(
        &json!({
            "spec": document(),
            "base_url": "https://api.example.com",
            "headers": { "X-Version": 1.0 },
        }),
        Arc::new(Registry::new()),
        false,
    )
    .await
    .expect_err("a non-string header value must not be silently dropped");
    let msg = err.to_string();
    assert!(
        msg.contains("mcp.openapi.headers.X-Version"),
        "unhelpful: {msg}"
    );
    assert!(msg.contains("must be a string"), "unhelpful: {msg}");
}

#[tokio::test]
async fn a_non_mapping_headers_value_is_a_startup_error() {
    let err = build_openapi_backend_from_config(
        &json!({
            "spec": document(),
            "base_url": "https://api.example.com",
            "headers": "X-Api-Key: secret",
        }),
        Arc::new(Registry::new()),
        false,
    )
    .await
    .expect_err("a scalar `headers` must not be accepted");
    assert!(
        err.to_string()
            .contains("mcp.openapi.headers must be a mapping"),
        "unhelpful: {err}"
    );
}

// ---------------------------------------------------------------------------
// #9 — `timeout` is the spec-fetch budget, not the proxy budget
// ---------------------------------------------------------------------------

#[tokio::test]
async fn documented_default_timeout_fetches_a_slow_spec() {
    let server = spawn_spec_server().await;
    // A forward test, not a regression one: Rust's shipped bug left the fetch
    // on apcore-toolkit's own 30 s default, which also clears 120 ms. It is
    // the TypeScript sibling of this case (`openapi-backend-wiring.test.ts`)
    // that fails without the fix, because there the default was 30 ms. Kept
    // here so the three SDKs assert the same default end-to-end.
    let registry = build_openapi_backend_from_config(
        &json!({ "spec": server.url, "base_url": "https://api.example.com" }),
        Arc::new(Registry::new()),
        false,
    )
    .await
    .expect("backend builds on the 30s default");
    assert!(registry_ids(&registry).contains(&"listpets".to_string()));
}

#[tokio::test]
async fn a_short_timeout_aborts_the_spec_fetch() {
    let server = spawn_spec_server().await;
    // 0.01 s = 10 ms, under the server's 120 ms. Before the fix this value
    // went to the proxy writer and the fetch ran on the toolkit default, so
    // the build SUCCEEDED — the timeout was unobservable at startup.
    let err = build_openapi_backend_from_config(
        &json!({
            "spec": server.url,
            "base_url": "https://api.example.com",
            "timeout": 0.01,
        }),
        Arc::new(Registry::new()),
        false,
    )
    .await
    .expect_err("a 10ms budget cannot fetch a 120ms spec");
    assert!(
        err.to_string().contains("failed to load spec"),
        "unexpected error: {err}"
    );
}

#[tokio::test]
async fn a_short_timeout_does_not_shrink_the_proxy_timeout() {
    // `timeout` is spec-fetch only, so a tiny value must leave PROXIED CALLS
    // on the 60 s default. Asserting the registry merely builds is not enough
    // — the shipped code built fine and failed later, at call time — so this
    // drives a real proxied request against a host that answers in 120 ms.
    // Before the fix that request inherited a 10 ms budget and timed out.
    let api = spawn_spec_server().await;
    let registry = build_openapi_backend_from_config(
        &json!({
            "spec": document(),
            "base_url": api.base,
            "timeout": 0.01,
        }),
        Arc::new(Registry::new()),
        false,
    )
    .await
    .expect("an already-parsed document needs no fetch");

    let module = registry
        .get("listpets")
        .expect("registry lookup")
        .expect("listpets registered");
    let ctx = Context::create(None, None, None, None, json!({}), None);
    let result = module.execute(json!({}), &ctx).await;
    assert!(
        result.is_ok(),
        "a proxied call must not inherit the spec-fetch budget: {result:?}"
    );
}

// ---------------------------------------------------------------------------
// #19 — a relative spec resolves against Config::project_root
// ---------------------------------------------------------------------------

/// A temp dir holding `apcore.yaml` (so it is `Config::project_root`) and the
/// spec file a relative `spec` must find inside it.
fn project_dir() -> tempfile::TempDir {
    let dir = tempfile::tempdir().expect("tempdir");
    // `Config::discover` VALIDATES: without `version` and `project.name` the
    // load fails, `config_project_root` degrades to CWD, and this test would
    // pass for the wrong reason.
    std::fs::write(
        dir.path().join("apcore.yaml"),
        "version: '0.15.0'\nproject:\n  name: wiring-test\n",
    )
    .unwrap();
    std::fs::write(
        dir.path().join("openapi.json"),
        serde_json::to_string(&document()).unwrap(),
    )
    .unwrap();
    dir
}

#[tokio::test]
async fn relative_spec_resolves_against_config_project_root() {
    let _guard = env_lock().lock().await;
    let dir = project_dir();
    // Safety: guarded by `env_lock`, and restored before the lock is released.
    unsafe { std::env::set_var("APCORE_CONFIG_FILE", dir.path().join("apcore.yaml")) };

    let result = build_openapi_backend_from_config(
        &json!({ "spec": "./openapi.json", "base_url": "https://api.example.com" }),
        Arc::new(Registry::new()),
        false,
    )
    .await;

    unsafe { std::env::remove_var("APCORE_CONFIG_FILE") };

    let registry = result.expect("the spec sits under project_root, not the CWD");
    assert!(registry_ids(&registry).contains(&"listpets".to_string()));
}

#[tokio::test]
async fn explicit_project_root_still_wins_over_config() {
    let _guard = env_lock().lock().await;
    let dir = project_dir();
    unsafe { std::env::set_var("APCORE_CONFIG_FILE", "/nonexistent/apcore.yaml") };

    let options = OpenAPIBackendOptions {
        base_url: Some("https://api.example.com".to_string()),
        project_root: Some(dir.path().to_string_lossy().into_owned()),
        ..OpenAPIBackendOptions::new()
    };
    let result =
        openapi_backend_from_spec("./openapi.json", Arc::new(Registry::new()), options).await;

    unsafe { std::env::remove_var("APCORE_CONFIG_FILE") };

    let registry = result.expect("an explicit project_root overrides Config");
    assert!(registry_ids(&registry).contains(&"listpets".to_string()));
}
