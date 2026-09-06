//! OpenAPI backend — serve an OpenAPI 3.0/3.1 document as MCP tools.
//!
//! Composes apcore-toolkit's shipped pieces into a populated [`Registry`]:
//!
//! ```text
//! load_spec -> OpenAPIScanner::scan -> HTTPProxyRegistryWriter::write -> Registry
//! ```
//!
//! and hands it to the machinery apcore-mcp already has. No scanning logic, no
//! schema conversion and no new execution path live here.
//!
//! See `apcore-mcp/docs/features/openapi-backend.md` for the specification and
//! `conformance/fixtures/openapi_backend.json` for the shared contract.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use apcore::Registry;
use apcore_toolkit::openapi_scanner::{OpenAPIScanner, ScanOptions};
use apcore_toolkit::output::http_proxy_writer::HTTPProxyRegistryWriter;
use apcore_toolkit::types::ScannedModule;
use serde_json::Value;

use crate::APCoreMCPError;

/// Methods that write — used only by the "nothing asks for approval" warning.
const WRITE_METHODS: [&str; 4] = ["POST", "PUT", "PATCH", "DELETE"];

const URL_SCHEMES: [&str; 2] = ["http://", "https://"];

/// Options for [`openapi_backend`].
#[derive(Default)]
pub struct OpenAPIBackendOptions {
    /// Where proxied requests go. Defaults to the document's `servers[0].url`.
    pub base_url: Option<String>,
    /// Prepended to every derived module ID. Required in a mixed deployment.
    pub prefix: Option<String>,
    /// Scanner include filter.
    pub include: Option<String>,
    /// Scanner exclude filter.
    pub exclude: Option<String>,
    /// When `false`, `deprecated: true` operations are skipped. Default `true`.
    pub include_deprecated: bool,
    /// Per-request auth headers for proxied calls (never the spec fetch).
    pub auth_header_factory: Option<Box<dyn Fn() -> HashMap<String, String> + Send + Sync>>,
    /// Proxy request timeout in seconds.
    pub timeout_secs: f64,
    /// True when another backend source is configured; makes `prefix` required.
    pub has_other_backend_source: bool,
    /// `Config::project_root` (apcore 0.30.0). A relative `spec` resolves here.
    pub project_root: Option<String>,
    /// Suppresses the "nothing will ask for approval" warning when the
    /// operator has deliberately reviewed and accepted it.
    pub acknowledge_unapproved_writes: bool,
}

impl OpenAPIBackendOptions {
    /// Options with every field at its documented default.
    #[must_use]
    pub fn new() -> Self {
        Self {
            include_deprecated: true,
            timeout_secs: 30.0,
            ..Default::default()
        }
    }
}

/// Whether one dot-separated segment is a legal apcore module-ID segment.
///
/// apcore's registry enforces `^[a-z][a-z0-9_]*(\.[a-z][a-z0-9_]*)*$` at
/// `Registry::register` and again at `Executor::call`; this is that pattern,
/// per segment, without pulling in a regex dependency.
#[must_use]
pub fn is_legal_segment(segment: &str) -> bool {
    let mut chars = segment.chars();
    match chars.next() {
        Some(c) if c.is_ascii_lowercase() => {}
        _ => return false,
    }
    chars.all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_')
}

/// Map a scanner-derived module ID into apcore's legal alphabet, or `None`.
///
/// apcore-toolkit's `derive_module_id` sanitizes to `[A-Za-z0-9_.-]`. apcore's
/// registry accepts only lowercase, digits, underscores and dots — "no
/// hyphens" — so the two alphabets differ and the scanner's output is not
/// directly registrable. Measured against apcore 0.30.0 and apcore-toolkit
/// 0.11.1, only two of nine realistic operation shapes register unrepaired,
/// and the canonical Swagger Petstore (`listPets`, `createPets`,
/// `showPetById`) is entirely in the rejected set: it scans cleanly, fails
/// registration on every operation as a per-module `WriteResult`, and yields an
/// **empty registry**.
///
/// The projection is lowercase, then `-` -> `_`. Both are mechanical and
/// lossless up to case. It deliberately stops there: a segment that still does
/// not begin with a lowercase letter (`/v1/2fa` -> `v1.2fa.post`) can only be
/// repaired by *inventing* a character, which is a naming decision that belongs
/// to the operator's own hook rather than to a silent default.
#[must_use]
pub fn project_module_id(module_id: &str) -> Option<String> {
    let candidate = module_id.to_ascii_lowercase().replace('-', "_");
    if candidate.is_empty() {
        return None;
    }
    if candidate.split('.').all(is_legal_segment) {
        Some(candidate)
    } else {
        None
    }
}

/// Resolve the `mcp.openapi.spec` value.
///
/// `spec` is the `mcp` namespace's first path-typed configuration key, and
/// apcore 0.30.0's protections for path-typed keys do **not** reach it:
/// `Config::path_typed_keys()` returns a hardcoded set of apcore's own keys and
/// never consults a namespace registered through `Config::register_namespace`,
/// and the PROTOCOL_SPEC §9.2.1 requirement-5 empty-value discard is gated on
/// that same set. So the three rules are the bridge's own:
///
/// 1. an `http(s)://` value is a URL, used **verbatim**;
/// 2. a set-but-empty value is discarded — the caller falls through to the
///    next configuration tier;
/// 3. a relative filesystem path resolves against `Config::project_root` —
///    §9.2.2's *target* semantics, adopted immediately because this key has
///    never shipped and so owes no deprecation window.
///
/// Returns `None` when the value was empty and the caller should fall through.
#[must_use]
pub fn resolve_spec_location(spec: &str, project_root: Option<&str>) -> Option<String> {
    if spec.trim().is_empty() {
        tracing::warn!(
            "mcp.openapi.spec is set but empty; it is path-typed and an empty string is not a \
             path (mirrors PROTOCOL_SPEC §9.2.1 requirement 5). Ignoring the value."
        );
        return None;
    }
    if URL_SCHEMES.iter().any(|s| spec.starts_with(s)) {
        return Some(spec.to_string());
    }
    let path = Path::new(spec);
    if path.is_absolute() {
        return Some(spec.to_string());
    }
    let base: PathBuf = project_root
        .map(PathBuf::from)
        .unwrap_or_else(|| std::env::current_dir().unwrap_or_else(|_| PathBuf::from(".")));
    Some(normalize(&base.join(path)))
}

/// Lexically normalize `.` / `..` without touching the filesystem, so the
/// result is comparable across the three SDKs for a path that need not exist.
fn normalize(path: &Path) -> String {
    let mut out: Vec<std::ffi::OsString> = Vec::new();
    for component in path.components() {
        match component {
            std::path::Component::CurDir => {}
            std::path::Component::ParentDir => {
                out.pop();
            }
            other => out.push(other.as_os_str().to_os_string()),
        }
    }
    let mut buf = PathBuf::new();
    for part in out {
        buf.push(part);
    }
    buf.to_string_lossy().into_owned()
}

/// Build a [`Registry`] from an already-parsed OpenAPI 3.0/3.1 document.
///
/// Build a [`Registry`] from a spec **location** — a URL, a filesystem path,
/// or an already-parsed document — resolving and fetching it first.
///
/// This is the entry point every other bridge's `openapi_backend` presents as
/// its single, polymorphic `spec` parameter (Python's `spec: Any`,
/// TypeScript's `spec: unknown`). Rust's lower-level [`openapi_backend`]
/// takes an already-parsed `&Value` only — this wrapper is what closes the
/// gap: it runs [`resolve_spec_location`], fetches/parses via
/// `apcore_toolkit::load_spec` when the result is a URL or path, and
/// delegates. Before this function existed, nothing in this crate's CLI or
/// builder ever called `openapi_backend` at all — not because the pieces
/// were missing, but because there was no single function a caller holding
/// only a spec string could call.
///
/// # Errors
///
/// Returns [`APCoreMCPError::Config`] when `spec` resolves to nothing (e.g. a
/// set-but-empty value with no fallback), when the spec cannot be fetched or
/// parsed, or for every error [`openapi_backend`] itself can raise.
pub async fn openapi_backend_from_spec(
    spec: &str,
    registry: Arc<Registry>,
    options: OpenAPIBackendOptions,
) -> Result<Arc<Registry>, APCoreMCPError> {
    let resolved = resolve_spec_location(spec, options.project_root.as_deref()).ok_or_else(|| {
        APCoreMCPError::Config("mcp.openapi.spec is required and resolved to nothing.".to_string())
    })?;

    // `load_spec` handles both branches (URL vs local path) and both
    // JSON/YAML parsing internally — no need to duplicate that here.
    let document = apcore_toolkit::openapi_scanner::load_spec(&resolved)
        .await
        .map_err(|e| APCoreMCPError::Config(format!("mcp.openapi: failed to load spec '{resolved}': {e}")))?;

    openapi_backend(&document, registry, options).await
}

/// Build a [`Registry`] from a Config Bus `mcp.openapi` mapping.
///
/// Mirrors `acl_builder::build_acl_from_config`: the raw `mcp.openapi`
/// Config Bus value is a plain JSON object (from `apcore.yaml` or
/// `APCORE_MCP_OPENAPI_*` env vars), not [`OpenAPIBackendOptions`] directly,
/// so this is the one place that translates between them.
///
/// `auth_header_factory` is deliberately not read from `openapi_config`: it
/// is a closure, and a Config Bus value sourced from YAML/JSON/env can never
/// carry one.
///
/// # Errors
///
/// Returns [`APCoreMCPError::Config`] when `openapi_config` carries no
/// `spec` key, or for every error [`openapi_backend_from_spec`] /
/// [`openapi_backend`] can raise.
pub async fn build_openapi_backend_from_config(
    openapi_config: &Value,
    registry: Arc<Registry>,
    has_other_backend_source: bool,
) -> Result<Arc<Registry>, APCoreMCPError> {
    let obj = openapi_config.as_object().ok_or_else(|| {
        APCoreMCPError::Config(format!(
            "mcp.openapi must be a mapping, got {}",
            value_type_name(openapi_config)
        ))
    })?;
    let spec = obj
        .get("spec")
        .ok_or_else(|| APCoreMCPError::Config("mcp.openapi.spec is required when mcp.openapi is configured".to_string()))?;

    let options = OpenAPIBackendOptions {
        base_url: obj.get("base_url").and_then(Value::as_str).map(String::from),
        prefix: obj.get("prefix").and_then(Value::as_str).map(String::from),
        include: obj.get("include").and_then(Value::as_str).map(String::from),
        exclude: obj.get("exclude").and_then(Value::as_str).map(String::from),
        include_deprecated: obj.get("include_deprecated").and_then(Value::as_bool).unwrap_or(true),
        auth_header_factory: None,
        timeout_secs: obj.get("timeout").and_then(Value::as_f64).unwrap_or(30.0),
        has_other_backend_source,
        project_root: None,
        acknowledge_unapproved_writes: obj
            .get("acknowledge_unapproved_writes")
            .and_then(Value::as_bool)
            .unwrap_or(false),
    };

    match spec {
        Value::String(s) => openapi_backend_from_spec(s, registry, options).await,
        document => openapi_backend(document, registry, options).await,
    }
}

fn value_type_name(v: &Value) -> &'static str {
    match v {
        Value::Null => "null",
        Value::Bool(_) => "bool",
        Value::Number(_) => "number",
        Value::String(_) => "string",
        Value::Array(_) => "array",
        Value::Object(_) => "object",
    }
}

/// See `Contract: openapi_backend` in
/// `apcore-mcp/docs/features/openapi-backend.md`.
///
/// # Errors
///
/// Returns [`APCoreMCPError::Config`] when a prefix is required and absent,
/// when the document is not scannable, when no base URL can be determined, or
/// when a derived module ID collides with one already in `registry`.
pub async fn openapi_backend(
    document: &Value,
    registry: Arc<Registry>,
    options: OpenAPIBackendOptions,
) -> Result<Arc<Registry>, APCoreMCPError> {
    if options.has_other_backend_source && options.prefix.is_none() {
        return Err(APCoreMCPError::Config(
            "mcp.openapi.prefix is required when an OpenAPI backend is combined with another \
             backend source: the scanner deduplicates IDs within one scan only and knows nothing \
             about modules already in the registry. Set --openapi-prefix / mcp.openapi.prefix."
                .to_string(),
        ));
    }

    // --- Scan -------------------------------------------------------------
    // Caller hook first, projection last. The order is normative: running the
    // projection last makes the invariant *every registered module ID is
    // apcore-legal* hold unconditionally. It also runs BEFORE the scanner's
    // `deduplicate_ids` — which happens after this callback — because
    // lowercasing can CREATE a collision the document did not have.
    let skipped: Arc<Mutex<Vec<(String, String)>>> = Arc::new(Mutex::new(Vec::new()));
    let skipped_hook = Arc::clone(&skipped);

    let mut scan_options = ScanOptions::new();
    scan_options.include = options.include.clone();
    scan_options.exclude = options.exclude.clone();
    scan_options.base_path_prefix = options.prefix.clone();
    scan_options.include_deprecated = options.include_deprecated;
    scan_options.transform_module = Some(Box::new(move |module: ScannedModule| {
        match project_module_id(&module.module_id) {
            Some(projected) => Some(ScannedModule {
                module_id: projected,
                ..module
            }),
            None => {
                let lowered = module.module_id.to_ascii_lowercase().replace('-', "_");
                let bad = lowered
                    .split('.')
                    .find(|s| !is_legal_segment(s))
                    .unwrap_or(&module.module_id)
                    .to_string();
                if let Ok(mut guard) = skipped_hook.lock() {
                    guard.push((module.module_id.clone(), bad));
                }
                None
            }
        }
    }));

    let modules = OpenAPIScanner::new()
        .scan(document, &scan_options)
        .await
        .map_err(|e| APCoreMCPError::Config(format!("mcp.openapi: {e}")))?;

    if let Ok(guard) = skipped.lock() {
        for (derived, segment) in guard.iter() {
            tracing::warn!(
                "OpenAPI operation skipped: derived module ID '{derived}' is not a legal apcore \
                 module ID — the segment '{segment}' does not match \
                 ^[a-z][a-z0-9_]*$. apcore's registry would refuse it. Supply a derive_module_id \
                 or transform_module hook to name this operation yourself."
            );
        }
    }
    for module in &modules {
        for warning in &module.warnings {
            tracing::warn!("OpenAPI scan warning for {}: {warning}", module.module_id);
        }
    }
    if modules.is_empty() {
        tracing::warn!(
            "OpenAPI document yielded zero modules; the server will start with no tools from it."
        );
    }

    // --- Collision preflight ----------------------------------------------
    let existing: Vec<String> = registry.list(None, None, Some(&["public", "hidden"]));
    let mut collisions: Vec<String> = modules
        .iter()
        .map(|m| m.module_id.clone())
        .filter(|id| existing.contains(id))
        .collect();
    collisions.sort_unstable();
    collisions.dedup();
    if !collisions.is_empty() {
        return Err(APCoreMCPError::Config(format!(
            "OpenAPI module IDs collide with modules already in the registry: {}. Nothing was \
             registered. Set or change mcp.openapi.prefix so the two ID spaces cannot overlap.",
            collisions.join(", ")
        )));
    }

    // --- Base URL ----------------------------------------------------------
    let base_url = options
        .base_url
        .clone()
        .or_else(|| document_server_url(document))
        .ok_or_else(|| {
            APCoreMCPError::Config(
                "mcp.openapi.base_url is required: the document declares no usable absolute \
                 servers[0].url, so every proxied call would resolve against an unknown host."
                    .to_string(),
            )
        })?;

    // --- Write -------------------------------------------------------------
    let writer = HTTPProxyRegistryWriter::new(
        base_url,
        options.auth_header_factory,
        if options.timeout_secs > 0.0 {
            options.timeout_secs
        } else {
            30.0
        },
    )
    .map_err(|e| APCoreMCPError::Config(format!("mcp.openapi: {e}")))?;

    for result in writer.write(&modules, &registry) {
        if let Some(err) = result.verification_error {
            tracing::error!(
                "OpenAPI module {} failed to register: {err}",
                result.module_id
            );
        }
    }

    if !options.acknowledge_unapproved_writes {
        warn_if_writes_have_no_approval_path(&modules);
    }
    Ok(registry)
}

fn document_server_url(document: &Value) -> Option<String> {
    document
        .get("servers")?
        .as_array()?
        .first()?
        .get("url")?
        .as_str()
        .filter(|u| URL_SCHEMES.iter().any(|s| u.starts_with(s)))
        .map(String::from)
}

/// Warn that nothing will ask for approval before a write.
///
/// The toolkit infers annotations from the HTTP method alone and never infers
/// `requires_approval`, so every scanned module arrives with it false: a
/// `POST /charges` that moves money is annotated exactly like a `POST /echo`.
///
/// This reports the **absence of an approval path, never the presence of
/// protection** — the rule apcore states on
/// `GovernanceState::unprotected_control_surface`: *"a wired ACL that permits
/// every call still yields false."* An attached ACL therefore does not
/// suppress it.
fn warn_if_writes_have_no_approval_path(modules: &[ScannedModule]) {
    let writes = modules
        .iter()
        .filter(|m| {
            let method = m
                .metadata
                .get("http_method")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_ascii_uppercase();
            WRITE_METHODS.contains(&method.as_str()) && !m.annotations.as_ref().is_some_and(|a| a.requires_approval)
        })
        .count();
    if writes == 0 {
        return;
    }
    tracing::warn!(
        "{writes} OpenAPI operation(s) use a write method (POST/PUT/PATCH/DELETE) and declare \
         requires_approval=false — the approval gate will not fire for any of them. The scanner \
         cannot know which operations are consequential and does not guess. Close it with an ACL \
         rule carrying `approval: required`, `gate_destructive` on the ExecutionPolicy, or a \
         transform_module hook that sets the annotation. Set \
         mcp.openapi.acknowledge_unapproved_writes: true to record this as a deliberate decision."
    );
}
