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
use std::sync::Arc;

use apcore::Registry;
use apcore_toolkit::openapi_scanner::{OpenAPIScanner, ScanOptions};
use apcore_toolkit::output::http_proxy_writer::HTTPProxyRegistryWriter;
use apcore_toolkit::types::ScannedModule;
use serde_json::Value;

use crate::APCoreMCPError;

/// Methods that write — used only by the "nothing asks for approval" warning.
const WRITE_METHODS: [&str; 4] = ["POST", "PUT", "PATCH", "DELETE"];

const URL_SCHEMES: [&str; 2] = ["http://", "https://"];

/// Proxy request timeout, in seconds, applied to every derived module.
///
/// `mcp.openapi.timeout` is spec-fetch only (`docs/features/openapi-backend.md`
/// line 379), so the proxy side is not configurable and takes a fixed default.
/// Python and TypeScript get theirs by omitting the argument entirely and
/// letting apcore-toolkit's own default apply (60.0 s / `60_000` ms);
/// `HTTPProxyRegistryWriter::new` in Rust takes the timeout positionally, has
/// no default, and rejects a non-positive value, so the same number is spelled
/// out here to keep the three SDKs on one value.
const PROXY_TIMEOUT_SECS: f64 = 60.0;

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
    /// Extra headers for the **spec fetch only** — never sent with proxied
    /// calls. `mcp.openapi.headers` on the Config Bus, `--openapi-header` on
    /// the CLI.
    pub headers: Option<HashMap<String, String>>,
    /// Per-request auth headers for proxied calls (never the spec fetch).
    pub auth_header_factory: Option<Box<dyn Fn() -> HashMap<String, String> + Send + Sync>>,
    /// Spec-fetch timeout in seconds — `mcp.openapi.timeout`. **Not** the
    /// per-call proxy timeout, which is fixed at [`PROXY_TIMEOUT_SECS`].
    pub timeout_secs: f64,
    /// True when another backend source is configured; makes `prefix` required.
    pub has_other_backend_source: bool,
    /// Overrides the base a relative `spec` resolves against. `None` — the
    /// normal case, including both the Config Bus and CLI routes — reads
    /// `Config::project_root` (apcore 0.30.0) in
    /// [`openapi_backend_from_spec`] instead.
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

/// Map a module ID into apcore's legal alphabet, or `None`.
///
/// The projection is lowercase, then `-` -> `_`. A segment that still does not
/// begin with a lowercase letter (`v1.2fa.post`) cannot be repaired without
/// inventing a character, so the result is `None`.
///
/// Deprecated: apcore-toolkit >= 0.13.0 emits every `module_id` in apcore's
/// Canonical ID alphabet itself — camelCase split into words (`listPets` ->
/// `list_pets`), `-` and other characters replaced with `_`, a legal ID never
/// rewritten — so this projection is no longer needed, and nothing in
/// apcore-mcp calls it any more. It is kept, with its behaviour unchanged, for
/// callers that imported it, and will be removed in a later minor release.
/// It does NOT agree with the toolkit: it lowercases without splitting words
/// (`listPets` -> `listpets`).
#[deprecated(
    note = "apcore-toolkit >= 0.13.0 emits every module ID in apcore's Canonical ID alphabet, so \
            the projection is no longer needed; it will be removed in a later minor release"
)]
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

/// The first segment of `module_id` apcore's registry would refuse, or `None`
/// when every segment is legal. An empty ID yields the empty segment, which is
/// illegal too.
fn illegal_segment(module_id: &str) -> Option<&str> {
    module_id.split('.').find(|s| !is_legal_segment(s))
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
    // `Config::project_root` is resolved HERE rather than at each call site: a
    // relative `spec` must resolve against the project root on every route (the
    // Config Bus, the CLI, and a direct call), and every route but a caller
    // passing `project_root` explicitly reaches this one function. Resolving
    // per call site is what left the Config Bus and CLI routes on CWD.
    let project_root = options.project_root.clone().or_else(config_project_root);
    let resolved = resolve_spec_location(spec, project_root.as_deref()).ok_or_else(|| {
        APCoreMCPError::Config("mcp.openapi.spec is required and resolved to nothing.".to_string())
    })?;

    // `load_spec_with_options` handles both branches (URL vs local path) and
    // both JSON/YAML parsing internally — no need to duplicate that here. The
    // options-taking variant is the one that carries `headers` and the
    // spec-fetch `timeout`; the zero-argument `load_spec` silently drops both.
    // `auth_header_factory` is deliberately NOT forwarded: it is the proxied-
    // call credential, and the spec fetch takes `headers` only — matching
    // Python (`openapi_backend.py:202`) and TypeScript (`openapi-backend.ts`).
    let load_options = apcore_toolkit::openapi_scanner::LoadSpecOptions {
        headers: options.headers.clone(),
        auth_header_factory: None,
        timeout_secs: options.timeout_secs,
    };
    let document =
        apcore_toolkit::openapi_scanner::load_spec_with_options(&resolved, &load_options)
            .await
            .map_err(|e| {
                APCoreMCPError::Config(format!(
                    "mcp.openapi: failed to load spec '{resolved}': {e}"
                ))
            })?;

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
    let spec = obj.get("spec").ok_or_else(|| {
        APCoreMCPError::Config(
            "mcp.openapi.spec is required when mcp.openapi is configured".to_string(),
        )
    })?;

    let options = OpenAPIBackendOptions {
        base_url: obj
            .get("base_url")
            .and_then(Value::as_str)
            .map(String::from),
        prefix: obj.get("prefix").and_then(Value::as_str).map(String::from),
        include: obj.get("include").and_then(Value::as_str).map(String::from),
        exclude: obj.get("exclude").and_then(Value::as_str).map(String::from),
        include_deprecated: obj
            .get("include_deprecated")
            .and_then(Value::as_bool)
            .unwrap_or(true),
        headers: headers_from_config(obj.get("headers")).map_err(APCoreMCPError::Config)?,
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

/// Read a Config Bus `headers` mapping into the option field.
///
/// A non-string value is a startup error, not a silent drop and not a guess at
/// its spelling. `X-Version: 1.0` in YAML is a *number*, and the three SDKs
/// cannot agree on what it means: Python's httpx raises `TypeError: Header
/// value must be str or bytes`, and TypeScript's `fetch` quietly coerces it to
/// `"1"` — changing the value the operator wrote. Failing here matches Python,
/// which is the reference implementation, and above all avoids the third
/// option: dropping the header while the operator watches an authenticated
/// spec fetch fail with the key sitting right there in `apcore.yaml`. That is
/// the exact failure this key's own bug report was about (#8).
fn headers_from_config(value: Option<&Value>) -> Result<Option<HashMap<String, String>>, String> {
    let Some(value) = value.filter(|v| !v.is_null()) else {
        return Ok(None);
    };
    let obj = value.as_object().ok_or_else(|| {
        format!(
            "mcp.openapi.headers must be a mapping, got {}",
            value_type_name(value)
        )
    })?;

    let mut map = HashMap::with_capacity(obj.len());
    for (key, raw) in obj {
        let text = raw.as_str().ok_or_else(|| {
            format!(
                "mcp.openapi.headers.{key} must be a string, got {} — quote it in YAML                  (`{key}: \"...\"`) so it is not parsed as a number or boolean",
                value_type_name(raw)
            )
        })?;
        map.insert(key.clone(), text.to_string());
    }
    Ok(if map.is_empty() { None } else { Some(map) })
}

/// Read `Config::project_root` (apcore 0.30.0), or `None` to fall back to CWD.
///
/// Mirrors Python's `_resolve_project_root` (`openapi_backend.py:128-141`):
/// the Config Bus route owns this lookup, because a caller reaching it holds
/// no `Config` of its own. Any failure degrades to CWD rather than aborting
/// startup — the base is a convenience, and a spec that resolves under CWD is
/// what every pre-0.30.0 deployment already had.
fn config_project_root() -> Option<String> {
    match apcore::config::Config::discover() {
        Ok(config) => {
            let root = config.project_root();
            root.to_str().filter(|s| !s.is_empty()).map(str::to_string)
        }
        Err(e) => {
            tracing::debug!("Config::project_root unavailable ({e}); falling back to CWD");
            None
        }
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
    // apcore-toolkit >= 0.13.0 normalises every module ID into apcore's
    // Canonical ID alphabet itself — after `base_path_prefix` and both
    // ID-affecting hooks, and before its own `deduplicate_ids` — so the bridge
    // applies no projection of its own and installs no hook.
    let mut scan_options = ScanOptions::new();
    scan_options.include = options.include.clone();
    scan_options.exclude = options.exclude.clone();
    scan_options.base_path_prefix = options.prefix.clone();
    scan_options.include_deprecated = options.include_deprecated;

    let scanned = OpenAPIScanner::new()
        .scan(document, &scan_options)
        .await
        .map_err(|e| APCoreMCPError::Config(format!("mcp.openapi: {e}")))?;

    // --- Skip what apcore's registry would refuse (FR-OPENAPI-008) --------
    // Normalisation cannot repair a segment that begins with a digit
    // (`/v1/2fa` -> `v1.2fa.post`) or an empty ID from a hook; the scanner
    // still emits such a module, with a legality warning. The check runs
    // HERE, on the ID `scan` returned — never inside `transform_module`, which
    // sees the ID before the toolkit's final normalisation (a `PetStore`
    // prefix is still to be normalised there) — and before the preflight and
    // the writer, so a skipped module never becomes a failed WriteResult. Its
    // scan warnings, the toolkit's legality warning among them, are superseded
    // by the one skip warning below.
    let mut modules: Vec<ScannedModule> = Vec::with_capacity(scanned.len());
    for module in scanned {
        match illegal_segment(&module.module_id) {
            None => modules.push(module),
            Some(segment) => tracing::warn!(
                "OpenAPI operation skipped: module ID '{}' is not a legal apcore module ID — the \
                 segment '{segment}' does not match ^[a-z][a-z0-9_]*$. apcore's registry would \
                 refuse it. Supply a derive_module_id or transform_module hook to name this \
                 operation yourself.",
                module.module_id
            ),
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
    let writer =
        HTTPProxyRegistryWriter::new(base_url, options.auth_header_factory, PROXY_TIMEOUT_SECS)
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
            WRITE_METHODS.contains(&method.as_str())
                && !m.annotations.as_ref().is_some_and(|a| a.requires_approval)
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
