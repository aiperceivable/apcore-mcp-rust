//! Locating the shared cross-language conformance fixtures.
//!
//! The fixtures live in the apcore-mcp spec repository rather than here,
//! because all three bridges have to drive the same bytes for the comparison
//! to mean anything. This module is the single place that knows how to find
//! them, and — more importantly — the single place that decides what a
//! *missing* fixture means.
//!
//! Every conformance test used to answer that question with `eprintln!` and an
//! early `return`, which reports the test as **passing**. That is worse than
//! the Python and TypeScript bridges managed (both at least recorded a skip),
//! and it was the state on every CI run, because no workflow checked the spec
//! repo out at all. The suite was green while proving nothing.
//!
//! The answer now depends on where we are: warn and skip locally, where a
//! contributor may simply not have the spec repo; fail in CI, where a missing
//! fixture means the cross-language guarantee is unverified.

#![allow(dead_code)]

use std::path::PathBuf;

const FIXTURE_SUBPATH: [&str; 3] = ["apcore-mcp", "conformance", "fixtures"];
const ENV_OVERRIDE: &str = "APCORE_CONFORMANCE_FIXTURES";
const MAX_ASCENT: usize = 4;

/// Return the shared fixtures directory, or `None` when it is not present.
///
/// Resolution order:
///
/// 1. `APCORE_CONFORMANCE_FIXTURES` — an explicit directory, for layouts
///    neither convention below covers.
/// 2. Walking up from this crate, looking for `apcore-mcp/conformance/
///    fixtures`. One walk covers both layouts: the sibling checkout developers
///    use (`…/aipartnerup/apcore-mcp`) and CI, where the spec repo is checked
///    out *inside* the workspace because `actions/checkout` refuses to place a
///    repository outside it.
pub fn fixtures_dir() -> Option<PathBuf> {
    if let Ok(override_path) = std::env::var(ENV_OVERRIDE) {
        let candidate = PathBuf::from(override_path);
        return candidate.is_dir().then_some(candidate);
    }
    candidates()
        .into_iter()
        .find(|candidate| candidate.is_dir())
}

/// The candidate fixture directories, in resolution order.
///
/// Shared by the lookup and by [`search_report`] so the two cannot drift — a
/// report that describes a different search than the one that ran is worse
/// than no report.
fn candidates() -> Vec<PathBuf> {
    let mut out = Vec::new();
    let mut dir: PathBuf = env!("CARGO_MANIFEST_DIR").into();
    for _ in 0..=MAX_ASCENT {
        out.push(
            FIXTURE_SUBPATH
                .iter()
                .fold(dir.clone(), |acc, part| acc.join(part)),
        );
        if !dir.pop() {
            break;
        }
    }
    out
}

/// Describe what the search actually saw.
///
/// A missing fixture is a hard failure in CI, so the message has to carry
/// enough to act on: whether the env override was in play, every path tried,
/// and — when a fixtures directory *was* found — what it holds instead. The
/// previous message stated the policy and reported no evidence, which left a
/// CI-only failure with nothing to diagnose from.
fn search_report(name: &str) -> String {
    let mut out = String::new();
    match std::env::var(ENV_OVERRIDE) {
        Ok(value) => {
            let verdict = if PathBuf::from(&value).is_dir() {
                "directory exists"
            } else {
                "NOT a directory - when this is set the ancestor walk is skipped entirely"
            };
            out.push_str(&format!(
                "  ${ENV_OVERRIDE} = {value:?} ({verdict})
"
            ));
        }
        Err(_) => out.push_str(&format!(
            "  ${ENV_OVERRIDE}: unset
"
        )),
    }
    out.push_str(&format!(
        "  ancestors of {} searched for {}:
",
        env!("CARGO_MANIFEST_DIR"),
        FIXTURE_SUBPATH.join("/"),
    ));
    for candidate in candidates() {
        let status = if !candidate.is_dir() {
            "no such directory".to_string()
        } else if candidate.join(name).is_file() {
            "HAS the fixture (it should have loaded - report this)".to_string()
        } else {
            format!("directory exists but holds no {name:?}; contains [{}]", {
                let mut names: Vec<String> = std::fs::read_dir(&candidate)
                    .map(|rd| {
                        rd.filter_map(Result::ok)
                            .map(|e| e.file_name().to_string_lossy().into_owned())
                            .collect()
                    })
                    .unwrap_or_default();
                names.sort();
                names.join(", ")
            })
        };
        out.push_str(&format!(
            "    - {} -> {status}
",
            candidate.display()
        ));
    }
    out
}

/// Load a conformance fixture by file name.
///
/// Returns `None` when the fixtures are simply absent locally, so the caller
/// can return early. Panics in CI, where absence means the suite proves
/// nothing.
///
/// A fixture that is present but malformed always panics, in CI or not. The
/// previous loader ended in `.ok()?`, which turned a parse error into the same
/// `None` as a missing file — so a corrupt or renamed field silently downgraded
/// the whole suite to a skip instead of failing.
pub fn load_fixture<T: serde::de::DeserializeOwned>(name: &str) -> Option<T> {
    if let Some(dir) = fixtures_dir() {
        let path = dir.join(name);
        if path.is_file() {
            let raw = std::fs::read_to_string(&path).unwrap_or_else(|e| {
                panic!("conformance fixture {} unreadable: {e}", path.display())
            });
            return Some(serde_json::from_str(&raw).unwrap_or_else(|e| {
                panic!("conformance fixture {} is malformed: {e}", path.display())
            }));
        }
    }

    let detail = format!(
        "conformance fixture {name:?} not found.\n{}",
        search_report(name)
    );

    assert!(
        std::env::var_os("CI").is_none(),
        "{detail}In CI this is a failure rather than a skip: the \
         cross-language conformance suite exists to catch divergence between \
         the three bridges, and skipping it silently reports success while \
         proving nothing. The workflow must check out aiperceivable/apcore-mcp \
         to the `apcore-mcp` path."
    );

    eprintln!(
        "skipping: {detail}Check out aiperceivable/apcore-mcp alongside this \
         repository to run it."
    );
    None
}

/// Register a minimal module so a preflight has something to collide with.
///
/// Used by `openapi_backend_conformance` to seed the target registry before
/// the OpenAPI backend scans, which is how the `id_collision_against_registry`
/// case reaches the pre-write preflight at all.
#[allow(dead_code)]
pub fn register_stub(registry: &apcore::Registry, module_id: &str) {
    use apcore::module::ModuleAnnotations;
    use apcore::registry::ModuleDescriptor;
    use apcore::FunctionModule;
    use std::collections::HashMap;

    let module = FunctionModule::new::<_, ()>(
        ModuleAnnotations::default(),
        serde_json::json!({"type": "object"}),
        serde_json::json!({"type": "object"}),
        |_args, _ctx| Box::pin(async move { Ok(serde_json::json!({})) }),
    );
    let descriptor = ModuleDescriptor {
        module_id: module_id.to_string(),
        name: None,
        description: "stub".to_string(),
        documentation: None,
        input_schema: serde_json::json!({"type": "object"}),
        output_schema: serde_json::json!({"type": "object"}),
        version: "1.0.0".to_string(),
        tags: vec![],
        annotations: Some(ModuleAnnotations::default()),
        examples: vec![],
        metadata: HashMap::new(),
        display: None,
        sunset_date: None,
        dependencies: vec![],
        enabled: true,
    };
    registry
        .register(module_id, Box::new(module), descriptor)
        .unwrap_or_else(|e| panic!("stub {module_id} failed to register: {e}"));
}
