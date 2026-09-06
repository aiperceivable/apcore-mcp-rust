//! Cross-language conformance: the OpenAPI backend.
//!
//! Drives the Rust implementation from the shared fixture at
//! `apcore-mcp/conformance/fixtures/openapi_backend.json`. The Python and
//! TypeScript bridges run the same fixture through their own entry points.
//!
//! Three sections, each with its own shape: `test_cases` (document ->
//! modules), `config_cases` (how the `spec` value resolves), `error_cases`
//! (fatal configurations).

mod common;

use std::collections::HashMap;
use std::sync::Arc;

use apcore::Registry;
use apcore_mcp::openapi_backend::{openapi_backend, resolve_spec_location, OpenAPIBackendOptions};
use serde::Deserialize;
use serde_json::Value;

#[derive(Deserialize)]
struct Fixture {
    test_cases: Vec<ModuleCase>,
    config_cases: Vec<ConfigCase>,
    error_cases: Vec<ErrorCase>,
}

#[derive(Deserialize)]
struct ModuleCase {
    id: String,
    document: Value,
    #[serde(default)]
    options: HashMap<String, Value>,
    expected_modules: Vec<ExpectedModule>,
}

#[derive(Deserialize)]
struct ExpectedModule {
    module_id: String,
    #[serde(default)]
    mcp_annotations: Option<HashMap<String, bool>>,
    #[serde(default)]
    requires_approval: Option<bool>,
}

#[derive(Deserialize)]
struct ConfigCase {
    id: String,
    spec_value: String,
    #[serde(default)]
    spec_value_next_tier: Option<String>,
    project_root: String,
    expected_resolved_spec: String,
}

#[derive(Deserialize)]
struct ErrorCase {
    id: String,
    document: Value,
    #[serde(default)]
    options: HashMap<String, Value>,
    #[serde(default)]
    preexisting_registry_module_ids: Vec<String>,
    expected_error_substrings: Vec<String>,
    #[serde(default)]
    expected_registry_module_ids_after: Option<Vec<String>>,
}

fn to_options(raw: &HashMap<String, Value>) -> OpenAPIBackendOptions {
    let mut o = OpenAPIBackendOptions::new();
    o.has_other_backend_source = raw
        .get("additional_backend_source")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    o.base_url = raw
        .get("base_url")
        .and_then(Value::as_str)
        .map(String::from);
    o.prefix = raw.get("prefix").and_then(Value::as_str).map(String::from);
    o.include = raw.get("include").and_then(Value::as_str).map(String::from);
    o.exclude = raw.get("exclude").and_then(Value::as_str).map(String::from);
    o.include_deprecated = raw
        .get("include_deprecated")
        .and_then(Value::as_bool)
        .unwrap_or(true);
    o
}

fn registry_ids(registry: &Registry) -> Vec<String> {
    let mut ids = registry.list(None, None, Some(&["public", "hidden"]));
    ids.sort_unstable();
    ids
}

#[tokio::test]
async fn conformance_modules() {
    let Some(fixture) = common::load_fixture::<Fixture>("openapi_backend.json") else {
        return;
    };

    for case in &fixture.test_cases {
        let registry = Arc::new(Registry::new());
        let registry = openapi_backend(&case.document, registry, to_options(&case.options))
            .await
            .unwrap_or_else(|e| panic!("case {}: unexpected error: {e}", case.id));

        let mut expected: Vec<String> = case
            .expected_modules
            .iter()
            .map(|m| m.module_id.clone())
            .collect();
        expected.sort_unstable();
        assert_eq!(
            registry_ids(&registry),
            expected,
            "case {}: module set mismatch",
            case.id
        );

        for want in &case.expected_modules {
            let definition = registry
                .get_definition(&want.module_id)
                .unwrap_or_else(|_| {
                    panic!("case {}: no definition for {}", case.id, want.module_id)
                })
                .unwrap_or_else(|| {
                    panic!("case {}: no definition for {}", case.id, want.module_id)
                });
            let annotations = definition.annotations.clone().unwrap_or_default();

            if let Some(hints) = &want.mcp_annotations {
                // The MCP camelCase projection of apcore's annotations.
                let observed: HashMap<&str, bool> = HashMap::from([
                    ("readOnlyHint", annotations.readonly),
                    ("destructiveHint", annotations.destructive),
                    ("idempotentHint", annotations.idempotent),
                    ("openWorldHint", annotations.open_world),
                ]);
                for (key, value) in hints {
                    assert_eq!(
                        observed.get(key.as_str()),
                        Some(value),
                        "case {}/{}: {key}",
                        case.id,
                        want.module_id
                    );
                }
            }

            if let Some(expected_approval) = want.requires_approval {
                assert_eq!(
                    annotations.requires_approval, expected_approval,
                    "case {}/{}: requires_approval — the scanner never infers it, and this case \
                     pins the gap as a fact",
                    case.id, want.module_id
                );
            }
        }
    }
}

#[test]
fn conformance_spec_resolution() {
    let Some(fixture) = common::load_fixture::<Fixture>("openapi_backend.json") else {
        return;
    };

    for case in &fixture.config_cases {
        let mut resolved = resolve_spec_location(&case.spec_value, Some(&case.project_root));
        if resolved.is_none() {
            // Discarded: the caller falls through to the next configuration tier.
            let next = case.spec_value_next_tier.as_deref().unwrap_or_else(|| {
                panic!(
                    "case {}: value discarded but no next tier declared",
                    case.id
                )
            });
            resolved = resolve_spec_location(next, Some(&case.project_root));
        }
        assert_eq!(
            resolved.as_deref(),
            Some(case.expected_resolved_spec.as_str()),
            "case {}: resolved spec mismatch",
            case.id
        );
    }
}

#[tokio::test]
async fn conformance_error_cases() {
    let Some(fixture) = common::load_fixture::<Fixture>("openapi_backend.json") else {
        return;
    };

    for case in &fixture.error_cases {
        let registry = Arc::new(Registry::new());
        for module_id in &case.preexisting_registry_module_ids {
            common::register_stub(&registry, module_id);
        }

        let err = match openapi_backend(
            &case.document,
            Arc::clone(&registry),
            to_options(&case.options),
        )
        .await
        {
            Err(e) => format!("{e}"),
            Ok(_) => panic!(
                "case {}: expected a rejection but the build succeeded",
                case.id
            ),
        };
        for fragment in &case.expected_error_substrings {
            assert!(
                err.contains(fragment),
                "case {}: error message {:?} missing substring {:?}",
                case.id,
                err,
                fragment
            );
        }

        if let Some(expected_after) = &case.expected_registry_module_ids_after {
            let mut expected = expected_after.clone();
            expected.sort_unstable();
            assert_eq!(
                registry_ids(&registry),
                expected,
                "case {}: the preflight must register NOTHING",
                case.id
            );
        }
    }
}
