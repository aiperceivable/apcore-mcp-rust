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

    let mut dir: PathBuf = env!("CARGO_MANIFEST_DIR").into();
    for _ in 0..=MAX_ASCENT {
        let candidate = FIXTURE_SUBPATH
            .iter()
            .fold(dir.clone(), |acc, part| acc.join(part));
        if candidate.is_dir() {
            return Some(candidate);
        }
        if !dir.pop() {
            break;
        }
    }
    None
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
        "conformance fixture {name:?} not found: checked ${ENV_OVERRIDE} and every ancestor of {} for {}",
        env!("CARGO_MANIFEST_DIR"),
        FIXTURE_SUBPATH.join("/"),
    );

    assert!(
        std::env::var_os("CI").is_none(),
        "{detail}. In CI this is a failure rather than a skip: the \
         cross-language conformance suite exists to catch divergence between \
         the three bridges, and skipping it silently reports success while \
         proving nothing. The workflow must check out aiperceivable/apcore-mcp \
         to the `apcore-mcp` path."
    );

    eprintln!("skipping: {detail}. Check out aiperceivable/apcore-mcp alongside this repository to run it.");
    None
}
