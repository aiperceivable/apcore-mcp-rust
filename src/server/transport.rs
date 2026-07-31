//! TransportManager — manages MCP transport lifecycle (stdio, HTTP, SSE).
//!
//! Also exposes Prometheus metrics for observability.

use std::convert::Infallible;
use std::sync::Arc;

use axum::extract::Request;
use axum::response::{IntoResponse, Redirect};
use axum::routing::{get, post};
use axum::Router;
use hyper::StatusCode;
use serde_json::Value;
use tokio::io::{AsyncBufReadExt, AsyncRead, AsyncWrite, AsyncWriteExt, BufReader};

// ---------------------------------------------------------------------------
// Task 1: TransportError
// ---------------------------------------------------------------------------

/// Unified error type for all transport failure modes.
#[derive(Debug, thiserror::Error)]
pub enum TransportError {
    #[error("invalid host: {0}")]
    InvalidHost(String),

    #[error("port must be between 1 and 65535, got {0}")]
    InvalidPort(u16),

    #[error("I/O error: {0}")]
    Io(#[from] std::io::Error),

    #[error("failed to bind to {host}:{port}: {source}")]
    Bind {
        host: String,
        port: u16,
        source: hyper::Error,
    },

    #[error("server error: {0}")]
    Server(String),
}

// ---------------------------------------------------------------------------
// MetricsExporter trait
// ---------------------------------------------------------------------------

/// Trait for exporting server metrics.
pub trait MetricsExporter: Send + Sync {
    /// Export metrics in Prometheus text format.
    fn export_prometheus(&self) -> String;
}

/// Blanket adapter: a shared apcore [`MetricsCollector`] already exposes a
/// `export_prometheus()` method, so wrap it directly as a `MetricsExporter`.
impl MetricsExporter for apcore::observability::metrics::MetricsCollector {
    fn export_prometheus(&self) -> String {
        apcore::observability::metrics::MetricsCollector::export_prometheus(self)
    }
}

/// Trait for exporting usage summaries at the `/usage` endpoint.
pub trait UsageExporter: Send + Sync {
    /// Return a JSON summary of module usage (per-module plus per-caller
    /// breakdown) suitable for JSON serialisation at `/usage`.
    fn export_json(&self) -> Value;
}

impl UsageExporter for apcore::observability::usage::UsageCollector {
    fn export_json(&self) -> Value {
        let summaries = self.get_all_summaries();
        serde_json::json!({
            "modules": summaries
                .iter()
                .map(|s| serde_json::json!({
                    "module_id": s.module_id,
                    "call_count": s.call_count,
                    "error_count": s.error_count,
                    "avg_latency_ms": s.avg_latency_ms,
                    "unique_callers": s.unique_callers,
                    "trend": s.trend,
                }))
                .collect::<Vec<_>>()
        })
    }
}

// ---------------------------------------------------------------------------
// McpHandler trait
// ---------------------------------------------------------------------------

/// Trait for handling MCP JSON-RPC messages.
///
/// The transport layer delegates incoming messages to an `McpHandler`
/// implementation, which processes them and optionally returns a response.
/// Notifications (no `id` field) return `None`.
#[async_trait::async_trait]
pub trait McpHandler: Send + Sync {
    /// Handle an incoming JSON-RPC message.
    ///
    /// Returns `Some(response)` for requests, `None` for notifications.
    async fn handle_message(&self, message: Value) -> Option<Value>;
}

// ---------------------------------------------------------------------------
// HttpAuthConfig
// ---------------------------------------------------------------------------

/// Authentication configuration for HTTP transports.
#[derive(Default, Clone)]
pub struct HttpAuthConfig {
    /// Optional authenticator. When `None`, no auth middleware is applied.
    pub authenticator: Option<Arc<dyn crate::auth::protocol::Authenticator>>,
    /// Whether unauthenticated requests are rejected (`true`) or allowed (`false`).
    pub require_auth: bool,
    /// Explorer URL prefix for GET-only exemption (browsing exempt, POST requires auth).
    pub explorer_prefix: Option<String>,
    /// User-configured paths that bypass authentication entirely.
    pub exempt_paths: Option<std::collections::HashSet<String>>,
}

impl std::fmt::Debug for HttpAuthConfig {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("HttpAuthConfig")
            .field("authenticator", &self.authenticator.as_ref().map(|_| "..."))
            .field("require_auth", &self.require_auth)
            .field("explorer_prefix", &self.explorer_prefix)
            .field("exempt_paths", &self.exempt_paths)
            .finish()
    }
}

/// Apply [`AuthMiddlewareLayer`] to a router based on [`HttpAuthConfig`].
///
/// Returns the router unchanged if no authenticator is configured.
/// Validates that `exempt_paths` entries start with `/`.
pub(crate) fn apply_auth_layer(app: Router, auth_config: HttpAuthConfig) -> Router {
    let Some(auth) = auth_config.authenticator else {
        return app;
    };

    use crate::auth::middleware::AuthMiddlewareLayer;

    // Explorer browsing (GET) is exempt; execution (POST) requires auth.
    let mut get_prefixes = Vec::new();
    if let Some(prefix) = auth_config.explorer_prefix {
        get_prefixes.push(prefix.clone());
        get_prefixes.push(format!("{prefix}/"));
    }

    let mut layer = AuthMiddlewareLayer::new(auth)
        .require_auth(auth_config.require_auth)
        .exempt_get_prefixes(get_prefixes);

    // Forward user-configured exempt paths (normalize missing leading /).
    if let Some(paths) = auth_config.exempt_paths {
        let normalized: std::collections::HashSet<String> = paths
            .into_iter()
            .map(|p| {
                if p.starts_with('/') {
                    p
                } else {
                    tracing::warn!(
                        "exempt_paths entry '{}' missing leading '/', auto-prepending",
                        p
                    );
                    format!("/{p}")
                }
            })
            .collect();
        layer = layer.exempt_paths(normalized);
    }

    tracing::info!(
        "Authentication enabled (require_auth={})",
        auth_config.require_auth
    );

    app.layer(layer)
}

// ---------------------------------------------------------------------------
// Task 2: TransportManager struct
// ---------------------------------------------------------------------------

/// Manages the transport layer for the MCP server.
pub struct TransportManager {
    start_time: tokio::time::Instant,
    module_count: usize,
    metrics_exporter: Option<Arc<dyn MetricsExporter>>,
    usage_exporter: Option<Arc<dyn UsageExporter>>,
    /// Optional handler called when the transport detects a session
    /// disconnect / cancellation notification. Used by
    /// [`AsyncTaskBridge::cancel_session_tasks`] to forward cancellation
    /// to any tasks launched from that session.
    #[allow(clippy::type_complexity)]
    cancel_handler: Option<Arc<dyn Fn(&str) + Send + Sync>>,
}

impl TransportManager {
    /// Create a new transport manager with an optional metrics exporter.
    pub fn new(metrics_exporter: Option<Arc<dyn MetricsExporter>>) -> Self {
        Self {
            start_time: tokio::time::Instant::now(),
            module_count: 0,
            metrics_exporter,
            usage_exporter: None,
            cancel_handler: None,
        }
    }

    /// Install an optional usage exporter (surfaced at `/usage`).
    pub fn set_usage_exporter(&mut self, exporter: Option<Arc<dyn UsageExporter>>) {
        self.usage_exporter = exporter;
    }

    /// Install a cancellation handler invoked with the session id when the
    /// transport observes a client disconnect or explicit cancel. The
    /// handler typically forwards to
    /// [`AsyncTaskBridge::cancel_session_tasks`].
    #[allow(clippy::type_complexity)]
    pub fn set_cancel_handler(&mut self, handler: Option<Arc<dyn Fn(&str) + Send + Sync>>) {
        self.cancel_handler = handler;
    }

    /// Invoke the cancellation handler with the given session id.
    /// No-op when no handler is installed.
    pub fn notify_cancel(&self, session_id: &str) {
        if let Some(h) = &self.cancel_handler {
            h(session_id);
        }
    }

    /// Set the number of registered modules (for metrics).
    pub fn set_module_count(&mut self, count: usize) {
        self.module_count = count;
    }

    /// Get the current module count.
    pub fn module_count(&self) -> usize {
        self.module_count
    }

    /// Validate host and port parameters.
    fn validate_host_port(host: &str, port: u16) -> Result<(), TransportError> {
        if host.is_empty() {
            return Err(TransportError::InvalidHost(host.to_string()));
        }
        if port == 0 {
            return Err(TransportError::InvalidPort(port));
        }
        Ok(())
    }

    // -----------------------------------------------------------------------
    // Task 3: Health & Metrics
    // -----------------------------------------------------------------------

    /// Build the health check response payload.
    fn build_health_response(&self) -> HealthResponse {
        HealthResponse {
            status: "ok",
            uptime_seconds: self.start_time.elapsed().as_secs_f64(),
            module_count: self.module_count,
        }
    }

    /// Build the metrics response.
    ///
    /// Returns `Ok(body)` with Prometheus text when an exporter is configured,
    /// or `Err(())` when no exporter is available (caller should return 404).
    fn build_metrics_response(&self) -> Result<String, ()> {
        match &self.metrics_exporter {
            Some(exporter) => Ok(exporter.export_prometheus()),
            None => Err(()),
        }
    }

    /// Build the usage response.
    ///
    /// Returns `Ok(body)` with JSON when a usage exporter is configured,
    /// or `Err(())` otherwise.
    fn build_usage_response(&self) -> Result<Value, ()> {
        match &self.usage_exporter {
            Some(exporter) => Ok(exporter.export_json()),
            None => Err(()),
        }
    }

    /// Build an axum [`Router`] with `/health`, `/metrics`, and `/usage` GET routes.
    ///
    /// The returned router uses `Arc<TransportManager>` as shared state.
    /// `/usage` returns 404 when no `UsageExporter` is configured.
    pub fn health_metrics_router(self: &Arc<Self>) -> Router {
        let tm = Arc::clone(self);
        Router::new()
            .route("/health", get(health_handler))
            .route("/metrics", get(metrics_handler))
            .route("/usage", get(usage_handler))
            .with_state(tm)
    }

    // -----------------------------------------------------------------------
    // Task 4: stdio transport
    // -----------------------------------------------------------------------

    /// Run the MCP server over stdio transport (blocks until EOF on stdin).
    ///
    /// Reads line-delimited JSON-RPC from stdin and writes responses to stdout.
    pub async fn run_stdio(&self, handler: &dyn McpHandler) -> Result<(), TransportError> {
        tracing::info!("Starting stdio transport");
        self.run_stdio_with_io(tokio::io::stdin(), tokio::io::stdout(), handler)
            .await
    }

    /// Testable core of `run_stdio` — reads from any `AsyncRead`, writes to any `AsyncWrite`.
    ///
    /// Each line is parsed as JSON. Valid messages are dispatched to `handler`.
    /// If the handler returns `Some(response)`, it is written as a JSON line to the writer.
    /// Invalid JSON lines are logged and skipped. EOF causes a clean return.
    pub async fn run_stdio_with_io<R, W>(
        &self,
        reader: R,
        mut writer: W,
        handler: &dyn McpHandler,
    ) -> Result<(), TransportError>
    where
        R: AsyncRead + Unpin,
        W: AsyncWrite + Unpin,
    {
        let mut buf_reader = BufReader::new(reader);
        let mut line = String::new();

        loop {
            line.clear();
            let bytes_read = buf_reader.read_line(&mut line).await?;
            if bytes_read == 0 {
                tracing::info!("stdio: EOF reached, shutting down");
                return Ok(());
            }

            let trimmed = line.trim();
            if trimmed.is_empty() {
                continue;
            }

            let message: Value = match serde_json::from_str(trimmed) {
                Ok(v) => v,
                Err(e) => {
                    tracing::warn!("stdio: invalid JSON, skipping: {}", e);
                    continue;
                }
            };

            if let Some(response) = handler.handle_message(message).await {
                let mut response_bytes = serde_json::to_vec(&response)
                    .map_err(|e| TransportError::Server(e.to_string()))?;
                response_bytes.push(b'\n');
                writer.write_all(&response_bytes).await?;
                writer.flush().await?;
            }
        }
    }

    // -----------------------------------------------------------------------
    // Task 5: streamable-HTTP transport
    // -----------------------------------------------------------------------

    /// Build an axum [`Router`] for the streamable-HTTP transport.
    ///
    /// Includes `/health`, `/metrics`, and `/mcp` endpoints.
    /// The `/mcp` endpoint handles POST (JSON-RPC request/response),
    /// GET (per-connection SSE server→client stream, see
    /// [`streamable_http_get_handler`]), and DELETE (session termination).
    /// Extra routes are merged into the router if provided.
    pub fn build_streamable_http_app(
        self: &Arc<Self>,
        handler: Arc<dyn McpHandler>,
        extra_routes: Option<Router>,
    ) -> Router {
        // TODO: Per MCP spec, each client connection should get its own
        // session ID.  Currently a single ID is shared across all connections,
        // which means DELETE (session termination) affects all clients.
        // Requires a session store keyed by UUID.
        let session_id = uuid::Uuid::new_v4().to_string();
        let mcp_state = StreamableHttpState {
            handler,
            session_id,
        };

        let mcp_router = Router::new()
            .route(
                "/",
                post(streamable_http_post_handler)
                    .get(streamable_http_get_handler)
                    .delete(streamable_http_delete_handler),
            )
            .with_state(mcp_state);

        let mut app = self.health_metrics_router().nest("/mcp", mcp_router);

        if let Some(extra) = extra_routes {
            app = app.merge(extra);
        }

        // Axum's nest() does not match trailing slashes.  Add a fallback
        // redirect so that e.g. /explorer/ redirects to /explorer.
        app = app.fallback(trailing_slash_redirect);

        app
    }

    /// Run the MCP server over streamable-HTTP transport.
    ///
    /// Binds to the given host and port and serves until shutdown.
    pub async fn run_streamable_http(
        self: &Arc<Self>,
        handler: Arc<dyn McpHandler>,
        host: &str,
        port: u16,
        extra_routes: Option<Router>,
    ) -> Result<(), TransportError> {
        self.run_streamable_http_with_auth(
            handler,
            host,
            port,
            extra_routes,
            HttpAuthConfig::default(),
        )
        .await
    }

    /// Run the MCP server over streamable-HTTP transport with optional authentication.
    ///
    /// When `auth_config.authenticator` is provided, requests are authenticated
    /// via [`AuthMiddlewareLayer`](crate::auth::middleware::AuthMiddlewareLayer)
    /// which handles exempt paths, GET-only exemptions, `require_auth` flag,
    /// and identity propagation via task-local.
    pub async fn run_streamable_http_with_auth(
        self: &Arc<Self>,
        handler: Arc<dyn McpHandler>,
        host: &str,
        port: u16,
        extra_routes: Option<Router>,
        auth_config: HttpAuthConfig,
    ) -> Result<(), TransportError> {
        Self::validate_host_port(host, port)?;
        tracing::info!("Starting streamable-http transport on {}:{}", host, port);

        let app = self.build_streamable_http_app(handler, extra_routes);
        let app = apply_auth_layer(app, auth_config);

        let addr: std::net::SocketAddr = format!("{}:{}", host, port)
            .parse()
            .map_err(|_| TransportError::InvalidHost(host.to_string()))?;
        let listener = tokio::net::TcpListener::bind(addr).await?;
        axum::serve(listener, app)
            .await
            .map_err(TransportError::Io)?;
        Ok(())
    }

    // -----------------------------------------------------------------------
    // Task 6: SSE transport (deprecated)
    // -----------------------------------------------------------------------

    /// Build an axum [`Router`] for the SSE transport.
    ///
    /// Includes `/health`, `/metrics`, `/sse` (GET), and `/messages/` (POST).
    ///
    /// Each `GET /sse` opens an independent session: it is assigned a fresh
    /// session id, gets its own inbound queue, and receives only the responses
    /// to messages posted to `/messages/?sessionId=<its own id>`. Sessions are
    /// removed from the registry when the client disconnects. Mirrors the
    /// TypeScript SDK's `runSse`, which keys one `SSEServerTransport` per
    /// session id, and the Python SDK's `SseServerTransport.connect_sse`.
    #[deprecated(note = "SSE transport is deprecated. Use streamable-HTTP instead.")]
    pub fn build_sse_app(
        self: &Arc<Self>,
        handler: Arc<dyn McpHandler>,
        extra_routes: Option<Router>,
    ) -> Router {
        #[allow(deprecated)]
        self.build_sse_app_parts(handler, extra_routes).0
    }

    /// Build the SSE router and hand back the live-session registry alongside
    /// it.
    ///
    /// Deregistration is otherwise unobservable from outside: a leaked entry
    /// still answers `400` once its inbound receiver is dropped, so status
    /// codes alone cannot tell a removed session from a leaked one. Tests
    /// assert on the registry directly.
    #[deprecated(note = "SSE transport is deprecated. Use streamable-HTTP instead.")]
    fn build_sse_app_parts(
        self: &Arc<Self>,
        handler: Arc<dyn McpHandler>,
        extra_routes: Option<Router>,
    ) -> (Router, SseSessions) {
        let sse_state = SseState {
            handler,
            sessions: Arc::new(std::sync::Mutex::new(std::collections::HashMap::new())),
            manager: Arc::clone(self),
        };
        let sessions = Arc::clone(&sse_state.sessions);

        let sse_router = Router::new()
            .route("/sse", get(sse_stream_handler))
            .route("/messages/", post(sse_messages_handler))
            .with_state(sse_state);

        let mut app = self.health_metrics_router().merge(sse_router);

        if let Some(extra) = extra_routes {
            app = app.merge(extra);
        }

        (app, sessions)
    }

    /// Run the MCP server over SSE transport (deprecated).
    ///
    /// Mounts `/sse` (GET) for the event stream and `/messages/` (POST) for
    /// client-to-server messages. Logs a deprecation warning at startup.
    #[deprecated(note = "SSE transport is deprecated. Use streamable-HTTP instead.")]
    pub async fn run_sse(
        self: &Arc<Self>,
        handler: Arc<dyn McpHandler>,
        host: &str,
        port: u16,
        extra_routes: Option<Router>,
    ) -> Result<(), TransportError> {
        #[allow(deprecated)]
        self.run_sse_with_auth(handler, host, port, extra_routes, HttpAuthConfig::default())
            .await
    }

    /// Run the MCP server over SSE transport with optional authentication (deprecated).
    #[deprecated(note = "SSE transport is deprecated. Use streamable-HTTP instead.")]
    pub async fn run_sse_with_auth(
        self: &Arc<Self>,
        handler: Arc<dyn McpHandler>,
        host: &str,
        port: u16,
        extra_routes: Option<Router>,
        auth_config: HttpAuthConfig,
    ) -> Result<(), TransportError> {
        Self::validate_host_port(host, port)?;
        tracing::info!("Starting sse transport on {}:{}", host, port);
        tracing::warn!("SSE transport is deprecated. Use Streamable HTTP instead.");

        #[allow(deprecated)]
        let app = self.build_sse_app(handler, extra_routes);

        let app = apply_auth_layer(app, auth_config);

        let addr: std::net::SocketAddr = format!("{}:{}", host, port)
            .parse()
            .map_err(|_| TransportError::InvalidHost(host.to_string()))?;
        let listener = tokio::net::TcpListener::bind(addr).await?;
        axum::serve(listener, app)
            .await
            .map_err(TransportError::Io)?;
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// Health response struct
// ---------------------------------------------------------------------------

#[derive(serde::Serialize)]
struct HealthResponse {
    status: &'static str,
    uptime_seconds: f64,
    module_count: usize,
}

// ---------------------------------------------------------------------------
// Axum handlers
// ---------------------------------------------------------------------------

async fn health_handler(
    axum::extract::State(tm): axum::extract::State<Arc<TransportManager>>,
) -> axum::Json<HealthResponse> {
    axum::Json(tm.build_health_response())
}

async fn metrics_handler(
    axum::extract::State(tm): axum::extract::State<Arc<TransportManager>>,
) -> axum::response::Response {
    match tm.build_metrics_response() {
        Ok(body) => (
            StatusCode::OK,
            [("content-type", "text/plain; version=0.0.4; charset=utf-8")],
            body,
        )
            .into_response(),
        Err(()) => StatusCode::NOT_FOUND.into_response(),
    }
}

async fn usage_handler(
    axum::extract::State(tm): axum::extract::State<Arc<TransportManager>>,
) -> axum::response::Response {
    match tm.build_usage_response() {
        Ok(body) => (StatusCode::OK, axum::Json(body)).into_response(),
        Err(()) => StatusCode::NOT_FOUND.into_response(),
    }
}

// ---------------------------------------------------------------------------
// Streamable-HTTP state & handlers
// ---------------------------------------------------------------------------

/// Shared state for the streamable-HTTP MCP endpoint.
#[derive(Clone)]
struct StreamableHttpState {
    handler: Arc<dyn McpHandler>,
    session_id: String,
}

/// POST /mcp — accept a JSON-RPC request and return a JSON-RPC response.
async fn streamable_http_post_handler(
    axum::extract::State(state): axum::extract::State<StreamableHttpState>,
    axum::Json(body): axum::Json<Value>,
) -> axum::response::Response {
    match state.handler.handle_message(body).await {
        Some(response) => (
            StatusCode::OK,
            [
                ("content-type", "application/json"),
                ("mcp-session-id", state.session_id.as_str()),
            ],
            serde_json::to_string(&response).unwrap_or_default(),
        )
            .into_response(),
        None => (
            StatusCode::ACCEPTED,
            [("mcp-session-id", state.session_id.as_str())],
        )
            .into_response(),
    }
}

/// GET /mcp — per-connection SSE streaming endpoint.
///
/// [L-2] Opens a long-lived server→client Server-Sent Events stream for the
/// session (MCP Streamable HTTP transport). The stream first emits the
/// `endpoint` event carrying the session-scoped POST URL, then holds the
/// connection open with periodic keep-alive comments until the client
/// disconnects. Each GET gets its own independent stream — the previous
/// placeholder emitted one event and closed the connection immediately, which
/// prevented clients from receiving any server-initiated messages.
async fn streamable_http_get_handler(
    axum::extract::State(state): axum::extract::State<StreamableHttpState>,
) -> axum::response::Response {
    use axum::response::sse::{Event, KeepAlive, Sse};
    use std::time::Duration;

    let session_id = state.session_id.clone();

    // Per-connection channel: the first item is the `endpoint` event; the
    // stream then stays open. axum's `KeepAlive` injects SSE comment frames so
    // intermediaries do not close an otherwise-idle connection. When the client
    // disconnects, axum drops the response future and the spawned task's send
    // attempts fail, tearing the per-connection task down.
    let (tx, rx) = tokio::sync::mpsc::channel::<Result<Event, Infallible>>(16);
    tokio::spawn(async move {
        // Advertise the session-scoped POST endpoint (MCP `endpoint` event).
        let endpoint = Event::default()
            .event("endpoint")
            .data(format!("/mcp?sessionId={session_id}"));
        if tx.send(Ok(endpoint)).await.is_err() {
            return;
        }
        // Hold the stream open. The KeepAlive layer below handles liveness;
        // this loop simply keeps the sender alive until the receiver (client)
        // goes away, at which point `closed()` resolves and the task exits.
        tx.closed().await;
    });

    let stream = tokio_stream::wrappers::ReceiverStream::new(rx);
    Sse::new(stream)
        .keep_alive(
            KeepAlive::new()
                .interval(Duration::from_secs(15))
                .text("keep-alive"),
        )
        .into_response()
}

/// DELETE /mcp — session termination.
async fn streamable_http_delete_handler(
    axum::extract::State(_state): axum::extract::State<StreamableHttpState>,
) -> StatusCode {
    StatusCode::OK
}

// ---------------------------------------------------------------------------
// SSE transport state & handlers (deprecated)
// ---------------------------------------------------------------------------

/// Registry of live SSE sessions: session id → that connection's inbound queue.
///
/// Guarded by a `std::sync::Mutex` rather than tokio's: every critical section
/// is a single `HashMap` operation with no `.await` inside, and a synchronous
/// lock is what lets [`SseSessionGuard::drop`] deregister a session during a
/// panic unwind, where awaiting is impossible.
type SseSessions =
    Arc<std::sync::Mutex<std::collections::HashMap<String, tokio::sync::mpsc::Sender<Value>>>>;

/// Lock the session registry, recovering from a poisoned mutex.
///
/// Poisoning carries no meaning here: the map holds channel senders, and a
/// panic mid-`insert`/`remove` cannot leave a `HashMap` in a state that a
/// later `get`/`remove` misreads. Failing every subsequent connection over it
/// would turn one panicking session into a dead transport.
fn lock_sessions(
    sessions: &SseSessions,
) -> std::sync::MutexGuard<'_, std::collections::HashMap<String, tokio::sync::mpsc::Sender<Value>>>
{
    sessions
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

/// RAII teardown for one SSE session: deregisters it and fires the transport's
/// cancellation handler when the consumer task ends, however it ends.
///
/// The teardown used to be two statements after the dispatch loop, which a
/// panic inside `McpHandler::handle_message` unwound straight past — leaking
/// the registry entry for the lifetime of the process and skipping [TM-4]
/// cancellation for every task the session had launched. Python brackets the
/// same work in `try`/`finally` (`_scoped_session`); `Drop` is the Rust
/// equivalent, and the family spec requires RAII for resource cleanup.
struct SseSessionGuard {
    session_id: String,
    sessions: SseSessions,
    manager: Arc<TransportManager>,
}

impl SseSessionGuard {
    /// Register `session_id` against its inbound queue and return the guard
    /// that will remove it again. Registration happens before the consumer
    /// task starts so the advertised endpoint URL is usable immediately.
    fn register(
        session_id: String,
        sessions: SseSessions,
        manager: Arc<TransportManager>,
        inbound_tx: tokio::sync::mpsc::Sender<Value>,
    ) -> Self {
        lock_sessions(&sessions).insert(session_id.clone(), inbound_tx);
        Self {
            session_id,
            sessions,
            manager,
        }
    }
}

impl Drop for SseSessionGuard {
    fn drop(&mut self) {
        lock_sessions(&self.sessions).remove(&self.session_id);
        // [TM-4] Cancel any async tasks bound to the disconnecting session,
        // matching the TypeScript `transport.onclose` handler. The id matches
        // because every dispatched message was stamped with it — see
        // [`stamp_session_id`].
        self.manager.notify_cancel(&self.session_id);
        tracing::debug!(session_id = %self.session_id, "sse session closed");
    }
}

/// Shared state for the SSE transport.
///
/// The registry is the only shared piece; every connection owns its own inbound
/// queue and its own outbound event stream, so one client can never observe
/// another client's responses.
#[derive(Clone)]
struct SseState {
    handler: Arc<dyn McpHandler>,
    sessions: SseSessions,
    manager: Arc<TransportManager>,
}

/// Query parameters accepted by `POST /messages/`.
#[derive(serde::Deserialize)]
struct SseMessageQuery {
    #[serde(rename = "sessionId")]
    session_id: Option<String>,
}

/// Write the transport-minted session id onto an inbound client message as
/// `params._meta.sessionId`.
///
/// This is the Rust counterpart of the TypeScript SDK's `extra.sessionId`
/// (supplied by `SSEServerTransport`) and of the Python SDK's
/// `transport_session_var` ContextVar, which `_scoped_session` sets around the
/// server run. All three publish the *transport's own* id, not the client's:
/// the value keys [`AsyncTaskBridge::session_tasks`], and
/// `notifications/cancelled` already routes a caller-supplied key into
/// `cancel_session_tasks`, so a client permitted to choose its own session id
/// could name — and mass-cancel — another connection's tasks. The write is
/// therefore an unconditional **overwrite**, never a default-if-absent.
///
/// Messages whose `params` is absent or is not a JSON object are left alone:
/// the handler reads `params._meta` by key, so such a message can neither
/// carry a session id nor smuggle one in.
fn stamp_session_id(message: &mut Value, session_id: &str) {
    let Some(object) = message.as_object_mut() else {
        return;
    };
    let Some(params) = object.get_mut("params").and_then(Value::as_object_mut) else {
        return;
    };
    match params.get_mut("_meta") {
        // Preserve sibling keys such as `progressToken`.
        Some(Value::Object(meta)) => {
            meta.insert(
                "sessionId".to_string(),
                Value::String(session_id.to_string()),
            );
        }
        // Absent, or present but not an object — replace it wholesale so a
        // non-object `_meta` cannot shadow the id we are about to publish.
        _ => {
            params.insert(
                "_meta".to_string(),
                serde_json::json!({ "sessionId": session_id }),
            );
        }
    }
}

/// GET /sse — open a session-scoped server-sent event stream.
///
/// Emits the MCP `endpoint` event carrying this session's POST URL, then
/// streams the responses to messages posted for this session and nothing else.
/// A keep-alive comment is sent periodically so the stream is never silent and
/// intermediaries do not close an idle connection. Every dispatched message is
/// stamped with this connection's session id (see [`stamp_session_id`]) so
/// tasks submitted over it are bound to it and can be cancelled on disconnect.
/// When the client disconnects the session is removed from the registry and its
/// consumer task exits, so no zombie consumer is left behind to swallow a later
/// message. Removal is [`SseSessionGuard`]'s `Drop`, so it survives a panic in
/// the handler as well as an ordinary exit.
async fn sse_stream_handler(
    axum::extract::State(state): axum::extract::State<SseState>,
) -> axum::response::Response {
    use axum::response::sse::{Event, KeepAlive, Sse};
    use std::time::Duration;

    let session_id = uuid::Uuid::new_v4().to_string();

    // Inbound: messages posted to /messages/?sessionId=<this session>.
    let (inbound_tx, mut inbound_rx) = tokio::sync::mpsc::channel::<Value>(256);
    // Outbound: the SSE frames this connection — and only this one — receives.
    let (event_tx, event_rx) = tokio::sync::mpsc::channel::<Result<Event, Infallible>>(256);

    // Moved into the consumer task below: whichever way that task ends —
    // client disconnect, channel close, or a panic in the handler — dropping
    // the guard deregisters the session and fires the cancellation handler.
    let guard = SseSessionGuard::register(
        session_id.clone(),
        state.sessions.clone(),
        state.manager.clone(),
        inbound_tx,
    );

    let handler = state.handler.clone();
    let owned_session_id = session_id.clone();

    tokio::spawn(async move {
        let _session_guard = guard;
        let endpoint = Event::default()
            .event("endpoint")
            .data(format!("/messages/?sessionId={owned_session_id}"));
        if event_tx.send(Ok(endpoint)).await.is_ok() {
            loop {
                tokio::select! {
                    // The client went away: axum dropped the response body and
                    // with it the receiving half of the event channel.
                    _ = event_tx.closed() => break,
                    inbound = inbound_rx.recv() => {
                        let Some(mut message) = inbound else { break };
                        stamp_session_id(&mut message, &owned_session_id);
                        if let Some(response) = handler.handle_message(message).await {
                            let data = serde_json::to_string(&response).unwrap_or_default();
                            let event = Event::default().event("message").data(data);
                            if event_tx.send(Ok(event)).await.is_err() {
                                break;
                            }
                        }
                    }
                }
            }
        }
    });

    Sse::new(tokio_stream::wrappers::ReceiverStream::new(event_rx))
        .keep_alive(
            KeepAlive::new()
                .interval(Duration::from_secs(15))
                .text("keep-alive"),
        )
        .into_response()
}

/// POST /messages/?sessionId=... — accept a client-to-server JSON-RPC message.
///
/// The `sessionId` query parameter names the SSE stream that will receive the
/// response; it is the value advertised by that stream's `endpoint` event.
/// Returns 202 Accepted once queued, or 400 when the session is missing or
/// unknown — a message with nowhere to go must not be silently absorbed.
///
/// The two rejection bodies are deliberately distinct. `unknown session` means
/// the registry had no entry; `session closed` means the entry was there but
/// its stream went away between lookup and send. Collapsing them into one
/// string makes a registry leak untestable, because a leaked entry keeps
/// answering 400 through the second branch.
async fn sse_messages_handler(
    axum::extract::State(state): axum::extract::State<SseState>,
    axum::extract::Query(query): axum::extract::Query<SseMessageQuery>,
    axum::Json(body): axum::Json<Value>,
) -> axum::response::Response {
    let Some(session_id) = query.session_id else {
        return (
            StatusCode::BAD_REQUEST,
            "missing sessionId query parameter; use the URL from the endpoint event",
        )
            .into_response();
    };

    let sender = lock_sessions(&state.sessions).get(&session_id).cloned();
    let Some(sender) = sender else {
        return (StatusCode::BAD_REQUEST, "unknown session").into_response();
    };

    match sender.send(body).await {
        Ok(()) => StatusCode::ACCEPTED.into_response(),
        // The stream closed between lookup and send.
        Err(_) => (StatusCode::BAD_REQUEST, "session closed").into_response(),
    }
}

/// Fallback handler that strips a trailing slash and redirects.
///
/// Axum's `nest("/prefix", ...)` matches `/prefix` but not `/prefix/`.
/// This fallback redirects `/prefix/` → `/prefix` so both work.
/// Non-trailing-slash paths that don't match any route get a 404.
async fn trailing_slash_redirect(req: Request) -> axum::response::Response {
    let path = req.uri().path();
    if path.len() > 1 && path.ends_with('/') {
        let trimmed = path.trim_end_matches('/');
        Redirect::permanent(trimmed).into_response()
    } else {
        axum::http::StatusCode::NOT_FOUND.into_response()
    }
}

// ===========================================================================
// Tests
// ===========================================================================

#[cfg(test)]
mod tests {
    use super::*;
    use std::error::Error;

    // -----------------------------------------------------------------------
    // Mock McpHandler for testing
    // -----------------------------------------------------------------------

    /// A mock handler that echoes back the request wrapped in a response envelope.
    struct EchoHandler;

    #[async_trait::async_trait]
    impl McpHandler for EchoHandler {
        async fn handle_message(&self, message: Value) -> Option<Value> {
            // Simulate JSON-RPC: if the message has an "id", return a response.
            if message.get("id").is_some() {
                Some(serde_json::json!({
                    "jsonrpc": "2.0",
                    "id": message["id"],
                    "result": message.get("params").cloned().unwrap_or(Value::Null),
                }))
            } else {
                // Notification — no response.
                None
            }
        }
    }

    // -----------------------------------------------------------------------
    // Task 1 tests: TransportError
    // -----------------------------------------------------------------------

    #[test]
    fn transport_error_invalid_port_display() {
        let err = TransportError::InvalidPort(0);
        assert_eq!(err.to_string(), "port must be between 1 and 65535, got 0");
    }

    #[test]
    fn transport_error_invalid_host_display() {
        let err = TransportError::InvalidHost("".to_string());
        assert_eq!(err.to_string(), "invalid host: ");
    }

    #[test]
    fn transport_error_io_wraps_and_preserves_message() {
        let io_err = std::io::Error::new(std::io::ErrorKind::NotFound, "file gone");
        let err = TransportError::Io(io_err);
        assert_eq!(err.to_string(), "I/O error: file gone");
    }

    #[test]
    fn transport_error_io_from_conversion() {
        let io_err = std::io::Error::other("boom");
        let err: TransportError = io_err.into();
        assert!(matches!(err, TransportError::Io(_)));
    }

    #[test]
    fn transport_error_implements_std_error() {
        let err = TransportError::Server("oops".to_string());
        // Ensure it implements std::error::Error by using it as a trait object.
        let _: &dyn Error = &err;
    }

    #[test]
    fn transport_error_server_display() {
        let err = TransportError::Server("something broke".to_string());
        assert_eq!(err.to_string(), "server error: something broke");
    }

    // -----------------------------------------------------------------------
    // Task 2 tests: TransportManager struct
    // -----------------------------------------------------------------------

    #[test]
    fn transport_manager_new_without_exporter() {
        let tm = TransportManager::new(None);
        assert!(tm.metrics_exporter.is_none());
        assert_eq!(tm.module_count, 0);
    }

    #[test]
    fn transport_manager_new_with_exporter() {
        struct DummyExporter;
        impl MetricsExporter for DummyExporter {
            fn export_prometheus(&self) -> String {
                "dummy".to_string()
            }
        }
        let exporter: Arc<dyn MetricsExporter> = Arc::new(DummyExporter);
        let tm = TransportManager::new(Some(exporter));
        assert!(tm.metrics_exporter.is_some());
    }

    #[test]
    fn transport_manager_module_count_defaults_to_zero() {
        let tm = TransportManager::new(None);
        assert_eq!(tm.module_count(), 0);
    }

    #[test]
    fn transport_manager_set_module_count() {
        let mut tm = TransportManager::new(None);
        tm.set_module_count(5);
        assert_eq!(tm.module_count(), 5);
    }

    #[test]
    fn validate_host_port_rejects_empty_host() {
        let result = TransportManager::validate_host_port("", 8080);
        assert!(result.is_err());
        assert!(matches!(
            result.unwrap_err(),
            TransportError::InvalidHost(_)
        ));
    }

    #[test]
    fn validate_host_port_rejects_port_zero() {
        let result = TransportManager::validate_host_port("localhost", 0);
        assert!(result.is_err());
        assert!(matches!(
            result.unwrap_err(),
            TransportError::InvalidPort(0)
        ));
    }

    #[test]
    fn validate_host_port_accepts_valid() {
        assert!(TransportManager::validate_host_port("127.0.0.1", 8080).is_ok());
        assert!(TransportManager::validate_host_port("localhost", 1).is_ok());
        assert!(TransportManager::validate_host_port("0.0.0.0", 65535).is_ok());
    }

    // -----------------------------------------------------------------------
    // Task 3 tests: Health & Metrics
    // -----------------------------------------------------------------------

    #[test]
    fn build_health_response_returns_ok_status() {
        let tm = TransportManager::new(None);
        let resp = tm.build_health_response();
        assert_eq!(resp.status, "ok");
        assert!(resp.uptime_seconds >= 0.0);
        assert_eq!(resp.module_count, 0);
    }

    #[test]
    fn build_health_response_reflects_module_count() {
        let mut tm = TransportManager::new(None);
        tm.set_module_count(3);
        let resp = tm.build_health_response();
        assert_eq!(resp.module_count, 3);
    }

    #[test]
    fn build_metrics_response_without_exporter_returns_err() {
        let tm = TransportManager::new(None);
        assert!(tm.build_metrics_response().is_err());
    }

    #[test]
    fn build_metrics_response_with_exporter_returns_body() {
        struct MockExporter;
        impl MetricsExporter for MockExporter {
            fn export_prometheus(&self) -> String {
                "# HELP up\nup 1\n".to_string()
            }
        }
        let exporter: Arc<dyn MetricsExporter> = Arc::new(MockExporter);
        let tm = TransportManager::new(Some(exporter));
        let result = tm.build_metrics_response();
        assert!(result.is_ok());
        assert_eq!(result.unwrap(), "# HELP up\nup 1\n");
    }

    #[tokio::test]
    async fn health_handler_returns_json() {
        use axum::body::Body;
        use axum::http::Request;
        use tower::ServiceExt;

        let tm = Arc::new(TransportManager::new(None));
        let app = tm.health_metrics_router();

        let req = Request::builder()
            .uri("/health")
            .body(Body::empty())
            .unwrap();
        let resp = app.oneshot(req).await.unwrap();

        assert_eq!(resp.status(), StatusCode::OK);

        let body = axum::body::to_bytes(resp.into_body(), usize::MAX)
            .await
            .unwrap();
        let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(json["status"], "ok");
        assert_eq!(json["module_count"], 0);
        assert!(json["uptime_seconds"].as_f64().unwrap() >= 0.0);
    }

    #[tokio::test]
    async fn metrics_handler_returns_404_without_exporter() {
        use axum::body::Body;
        use axum::http::Request;
        use tower::ServiceExt;

        let tm = Arc::new(TransportManager::new(None));
        let app = tm.health_metrics_router();

        let req = Request::builder()
            .uri("/metrics")
            .body(Body::empty())
            .unwrap();
        let resp = app.oneshot(req).await.unwrap();

        assert_eq!(resp.status(), StatusCode::NOT_FOUND);
    }

    #[tokio::test]
    async fn metrics_handler_returns_prometheus_text_with_exporter() {
        use axum::body::Body;
        use axum::http::Request;
        use tower::ServiceExt;

        struct MockExporter;
        impl MetricsExporter for MockExporter {
            fn export_prometheus(&self) -> String {
                "# TYPE gauge\nmy_metric 42\n".to_string()
            }
        }

        let exporter: Arc<dyn MetricsExporter> = Arc::new(MockExporter);
        let tm = Arc::new(TransportManager::new(Some(exporter)));
        let app = tm.health_metrics_router();

        let req = Request::builder()
            .uri("/metrics")
            .body(Body::empty())
            .unwrap();
        let resp = app.oneshot(req).await.unwrap();

        assert_eq!(resp.status(), StatusCode::OK);
        let ct = resp
            .headers()
            .get("content-type")
            .unwrap()
            .to_str()
            .unwrap();
        assert_eq!(ct, "text/plain; version=0.0.4; charset=utf-8");

        let body = axum::body::to_bytes(resp.into_body(), usize::MAX)
            .await
            .unwrap();
        assert_eq!(
            String::from_utf8(body.to_vec()).unwrap(),
            "# TYPE gauge\nmy_metric 42\n"
        );
    }

    #[tokio::test]
    async fn health_handler_reflects_module_count() {
        use axum::body::Body;
        use axum::http::Request;
        use tower::ServiceExt;

        let mut tm = TransportManager::new(None);
        tm.set_module_count(7);
        let tm = Arc::new(tm);
        let app = tm.health_metrics_router();

        let req = Request::builder()
            .uri("/health")
            .body(Body::empty())
            .unwrap();
        let resp = app.oneshot(req).await.unwrap();

        let body = axum::body::to_bytes(resp.into_body(), usize::MAX)
            .await
            .unwrap();
        let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(json["module_count"], 7);
    }

    #[tokio::test]
    async fn streamable_http_get_emits_endpoint_event_and_stays_open() {
        // [L-2] GET /mcp must open a per-connection SSE stream that emits the
        // `endpoint` event and then stays open (held by KeepAlive) rather than
        // closing immediately.
        use axum::body::Body;
        use axum::http::Request;
        use http_body_util::BodyExt;
        use tower::ServiceExt;

        let tm = Arc::new(TransportManager::new(None));
        let handler: Arc<dyn McpHandler> = Arc::new(EchoHandler);
        let app = tm.build_streamable_http_app(handler, None);

        let req = Request::builder()
            .method("GET")
            .uri("/mcp")
            .body(Body::empty())
            .unwrap();
        let resp = app.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let ct = resp
            .headers()
            .get("content-type")
            .and_then(|v| v.to_str().ok())
            .unwrap_or_default()
            .to_string();
        assert!(
            ct.starts_with("text/event-stream"),
            "GET /mcp must be an SSE stream; content-type was {ct}"
        );

        // Read just the first frame; the stream stays open afterwards, so wrap
        // the read in a timeout (a closed-immediately stream would instead
        // yield None / end quickly).
        let mut body = resp.into_body();
        let frame = tokio::time::timeout(std::time::Duration::from_secs(2), body.frame())
            .await
            .expect("first SSE frame should arrive promptly")
            .expect("stream must yield a frame")
            .expect("frame must not be an error");
        let data = frame.into_data().expect("data frame");
        let text = String::from_utf8_lossy(&data);
        assert!(
            text.contains("event:endpoint") || text.contains("event: endpoint"),
            "first SSE frame must be the endpoint event; got: {text:?}"
        );
        assert!(
            text.contains("/mcp?sessionId="),
            "endpoint event must carry the session-scoped URL; got: {text:?}"
        );
    }

    // -----------------------------------------------------------------------
    // Task 4 tests: stdio transport
    // -----------------------------------------------------------------------

    #[tokio::test]
    async fn stdio_reads_jsonrpc_request_and_writes_response() {
        let input = r#"{"jsonrpc":"2.0","id":1,"method":"ping","params":{}}"#.to_string() + "\n";
        let reader = std::io::Cursor::new(input.into_bytes());
        let mut output = Vec::new();

        let tm = TransportManager::new(None);
        let handler = EchoHandler;
        let result = tm.run_stdio_with_io(reader, &mut output, &handler).await;

        assert!(result.is_ok());
        let response_str = String::from_utf8(output).unwrap();
        let response: Value = serde_json::from_str(response_str.trim()).unwrap();
        assert_eq!(response["jsonrpc"], "2.0");
        assert_eq!(response["id"], 1);
    }

    #[tokio::test]
    async fn stdio_eof_returns_ok() {
        let reader = std::io::Cursor::new(Vec::<u8>::new());
        let mut output = Vec::new();

        let tm = TransportManager::new(None);
        let handler = EchoHandler;
        let result = tm.run_stdio_with_io(reader, &mut output, &handler).await;

        assert!(result.is_ok());
        assert!(output.is_empty());
    }

    #[tokio::test]
    async fn stdio_invalid_json_is_skipped() {
        // First line is invalid, second line is valid.
        let input = "not-json\n{\"jsonrpc\":\"2.0\",\"id\":2,\"method\":\"test\"}\n".to_string();
        let reader = std::io::Cursor::new(input.into_bytes());
        let mut output = Vec::new();

        let tm = TransportManager::new(None);
        let handler = EchoHandler;
        let result = tm.run_stdio_with_io(reader, &mut output, &handler).await;

        assert!(result.is_ok());
        let response_str = String::from_utf8(output).unwrap();
        let response: Value = serde_json::from_str(response_str.trim()).unwrap();
        assert_eq!(response["id"], 2);
    }

    #[tokio::test]
    async fn stdio_notification_produces_no_output() {
        // Notification: no "id" field — handler returns None.
        let input = r#"{"jsonrpc":"2.0","method":"notify"}"#.to_string() + "\n";
        let reader = std::io::Cursor::new(input.into_bytes());
        let mut output = Vec::new();

        let tm = TransportManager::new(None);
        let handler = EchoHandler;
        let result = tm.run_stdio_with_io(reader, &mut output, &handler).await;

        assert!(result.is_ok());
        assert!(output.is_empty());
    }

    #[tokio::test]
    async fn stdio_multiple_requests() {
        let input = concat!(
            r#"{"jsonrpc":"2.0","id":1,"method":"a"}"#,
            "\n",
            r#"{"jsonrpc":"2.0","id":2,"method":"b"}"#,
            "\n",
        )
        .to_string();
        let reader = std::io::Cursor::new(input.into_bytes());
        let mut output = Vec::new();

        let tm = TransportManager::new(None);
        let handler = EchoHandler;
        tm.run_stdio_with_io(reader, &mut output, &handler)
            .await
            .unwrap();

        let output_str = String::from_utf8(output).unwrap();
        let lines: Vec<&str> = output_str.trim().split('\n').collect();
        assert_eq!(lines.len(), 2);
        let r1: Value = serde_json::from_str(lines[0]).unwrap();
        let r2: Value = serde_json::from_str(lines[1]).unwrap();
        assert_eq!(r1["id"], 1);
        assert_eq!(r2["id"], 2);
    }

    #[tokio::test]
    async fn stdio_empty_lines_are_skipped() {
        let input = "\n\n{\"jsonrpc\":\"2.0\",\"id\":1,\"method\":\"x\"}\n\n".to_string();
        let reader = std::io::Cursor::new(input.into_bytes());
        let mut output = Vec::new();

        let tm = TransportManager::new(None);
        let handler = EchoHandler;
        tm.run_stdio_with_io(reader, &mut output, &handler)
            .await
            .unwrap();

        let output_str = String::from_utf8(output).unwrap();
        let lines: Vec<&str> = output_str.trim().split('\n').collect();
        assert_eq!(lines.len(), 1);
    }

    // -----------------------------------------------------------------------
    // Task 5 tests: streamable-HTTP transport
    // -----------------------------------------------------------------------

    #[tokio::test]
    async fn streamable_http_app_health_returns_200() {
        use axum::body::Body;
        use axum::http::Request;
        use tower::ServiceExt;

        let tm = Arc::new(TransportManager::new(None));
        let handler: Arc<dyn McpHandler> = Arc::new(EchoHandler);
        let app = tm.build_streamable_http_app(handler, None);

        let req = Request::builder()
            .uri("/health")
            .body(Body::empty())
            .unwrap();
        let resp = app.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
    }

    #[tokio::test]
    async fn streamable_http_app_metrics_returns_404_without_exporter() {
        use axum::body::Body;
        use axum::http::Request;
        use tower::ServiceExt;

        let tm = Arc::new(TransportManager::new(None));
        let handler: Arc<dyn McpHandler> = Arc::new(EchoHandler);
        let app = tm.build_streamable_http_app(handler, None);

        let req = Request::builder()
            .uri("/metrics")
            .body(Body::empty())
            .unwrap();
        let resp = app.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::NOT_FOUND);
    }

    #[tokio::test]
    async fn streamable_http_post_mcp_returns_jsonrpc_response() {
        use axum::body::Body;
        use axum::http::Request;
        use tower::ServiceExt;

        let tm = Arc::new(TransportManager::new(None));
        let handler: Arc<dyn McpHandler> = Arc::new(EchoHandler);
        let app = tm.build_streamable_http_app(handler, None);

        let body = serde_json::json!({
            "jsonrpc": "2.0",
            "id": 42,
            "method": "tools/list",
            "params": {}
        });
        let req = Request::builder()
            .method("POST")
            .uri("/mcp")
            .header("content-type", "application/json")
            .body(Body::from(serde_json::to_vec(&body).unwrap()))
            .unwrap();
        let resp = app.oneshot(req).await.unwrap();

        assert_eq!(resp.status(), StatusCode::OK);
        assert!(resp.headers().get("mcp-session-id").is_some());

        let resp_body = axum::body::to_bytes(resp.into_body(), usize::MAX)
            .await
            .unwrap();
        let json: Value = serde_json::from_slice(&resp_body).unwrap();
        assert_eq!(json["jsonrpc"], "2.0");
        assert_eq!(json["id"], 42);
    }

    #[tokio::test]
    async fn streamable_http_post_notification_returns_202() {
        use axum::body::Body;
        use axum::http::Request;
        use tower::ServiceExt;

        let tm = Arc::new(TransportManager::new(None));
        let handler: Arc<dyn McpHandler> = Arc::new(EchoHandler);
        let app = tm.build_streamable_http_app(handler, None);

        // Notification: no "id" field.
        let body = serde_json::json!({
            "jsonrpc": "2.0",
            "method": "notifications/initialized"
        });
        let req = Request::builder()
            .method("POST")
            .uri("/mcp")
            .header("content-type", "application/json")
            .body(Body::from(serde_json::to_vec(&body).unwrap()))
            .unwrap();
        let resp = app.oneshot(req).await.unwrap();

        assert_eq!(resp.status(), StatusCode::ACCEPTED);
    }

    #[tokio::test]
    async fn streamable_http_delete_mcp_returns_200() {
        use axum::body::Body;
        use axum::http::Request;
        use tower::ServiceExt;

        let tm = Arc::new(TransportManager::new(None));
        let handler: Arc<dyn McpHandler> = Arc::new(EchoHandler);
        let app = tm.build_streamable_http_app(handler, None);

        let req = Request::builder()
            .method("DELETE")
            .uri("/mcp")
            .body(Body::empty())
            .unwrap();
        let resp = app.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
    }

    #[tokio::test]
    async fn streamable_http_extra_routes_are_merged() {
        use axum::body::Body;
        use axum::http::Request;
        use tower::ServiceExt;

        let tm = Arc::new(TransportManager::new(None));
        let handler: Arc<dyn McpHandler> = Arc::new(EchoHandler);

        let extra = Router::new().route("/custom", get(|| async { "custom-response" }));
        let app = tm.build_streamable_http_app(handler, Some(extra));

        let req = Request::builder()
            .uri("/custom")
            .body(Body::empty())
            .unwrap();
        let resp = app.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let body = axum::body::to_bytes(resp.into_body(), usize::MAX)
            .await
            .unwrap();
        assert_eq!(String::from_utf8(body.to_vec()).unwrap(), "custom-response");
    }

    #[tokio::test]
    async fn run_streamable_http_rejects_empty_host() {
        let tm = Arc::new(TransportManager::new(None));
        let handler: Arc<dyn McpHandler> = Arc::new(EchoHandler);
        let result = tm.run_streamable_http(handler, "", 8080, None).await;
        assert!(matches!(result, Err(TransportError::InvalidHost(_))));
    }

    #[tokio::test]
    async fn run_streamable_http_rejects_port_zero() {
        let tm = Arc::new(TransportManager::new(None));
        let handler: Arc<dyn McpHandler> = Arc::new(EchoHandler);
        let result = tm.run_streamable_http(handler, "localhost", 0, None).await;
        assert!(matches!(result, Err(TransportError::InvalidPort(0))));
    }

    #[tokio::test]
    async fn streamable_http_get_mcp_returns_sse() {
        use axum::body::Body;
        use axum::http::Request;
        use tower::ServiceExt;

        let tm = Arc::new(TransportManager::new(None));
        let handler: Arc<dyn McpHandler> = Arc::new(EchoHandler);
        let app = tm.build_streamable_http_app(handler, None);

        let req = Request::builder().uri("/mcp").body(Body::empty()).unwrap();
        let resp = app.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let ct = resp
            .headers()
            .get("content-type")
            .unwrap()
            .to_str()
            .unwrap();
        assert!(ct.contains("text/event-stream"));
    }

    // -----------------------------------------------------------------------
    // Task 6 tests: SSE transport (deprecated)
    // -----------------------------------------------------------------------

    #[tokio::test]
    async fn sse_app_health_returns_200() {
        use axum::body::Body;
        use axum::http::Request;
        use tower::ServiceExt;

        let tm = Arc::new(TransportManager::new(None));
        let handler: Arc<dyn McpHandler> = Arc::new(EchoHandler);
        #[allow(deprecated)]
        let app = tm.build_sse_app(handler, None);

        let req = Request::builder()
            .uri("/health")
            .body(Body::empty())
            .unwrap();
        let resp = app.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
    }

    #[tokio::test]
    async fn sse_app_metrics_returns_404_without_exporter() {
        use axum::body::Body;
        use axum::http::Request;
        use tower::ServiceExt;

        let tm = Arc::new(TransportManager::new(None));
        let handler: Arc<dyn McpHandler> = Arc::new(EchoHandler);
        #[allow(deprecated)]
        let app = tm.build_sse_app(handler, None);

        let req = Request::builder()
            .uri("/metrics")
            .body(Body::empty())
            .unwrap();
        let resp = app.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::NOT_FOUND);
    }

    /// Open a `GET /sse` stream and return its body plus the session id read
    /// off the mandatory `endpoint` event.
    async fn open_sse_session(app: &Router) -> (axum::body::Body, String) {
        use axum::body::Body;
        use axum::http::Request;
        use tower::ServiceExt;

        let req = Request::builder().uri("/sse").body(Body::empty()).unwrap();
        let resp = app.clone().oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let mut body = resp.into_body();
        let text = read_sse_chunk(&mut body).await;
        assert!(
            text.contains("event:endpoint") || text.contains("event: endpoint"),
            "first SSE frame must be the endpoint event; got: {text:?}"
        );
        let session_id = text
            .split("sessionId=")
            .nth(1)
            .and_then(|rest| rest.split_whitespace().next())
            .expect("endpoint event must carry sessionId")
            .to_string();
        (body, session_id)
    }

    /// Read the next data chunk from an SSE body as UTF-8 text.
    async fn read_sse_chunk(body: &mut axum::body::Body) -> String {
        use http_body_util::BodyExt;

        let frame = tokio::time::timeout(std::time::Duration::from_secs(2), body.frame())
            .await
            .expect("SSE frame should arrive promptly")
            .expect("stream must yield a frame")
            .expect("frame must not be an error");
        let data = frame.into_data().expect("data frame");
        String::from_utf8_lossy(&data).to_string()
    }

    /// Collect the next `count` JSON-RPC responses carried by `message` events.
    async fn collect_sse_responses(body: &mut axum::body::Body, count: usize) -> Vec<Value> {
        let mut responses = Vec::new();
        while responses.len() < count {
            let chunk = read_sse_chunk(body).await;
            for line in chunk.lines() {
                let Some(payload) = line.strip_prefix("data:") else {
                    continue;
                };
                let Ok(value) = serde_json::from_str::<Value>(payload.trim()) else {
                    continue;
                };
                if value.get("id").is_some() {
                    responses.push(value);
                }
            }
        }
        responses
    }

    /// Collect the JSON-RPC ids carried by the next `count` `message` events.
    async fn collect_sse_response_ids(body: &mut axum::body::Body, count: usize) -> Vec<i64> {
        collect_sse_responses(body, count)
            .await
            .iter()
            .filter_map(|v| v.get("id").and_then(Value::as_i64))
            .collect()
    }

    /// POST a JSON-RPC request bound to a specific SSE session.
    async fn post_sse_message(app: &Router, session_id: &str, id: i64) -> StatusCode {
        post_sse_message_with_params(app, session_id, id, Value::Null)
            .await
            .0
    }

    /// POST a JSON-RPC request carrying explicit `params` to a specific
    /// SSE session. `Value::Null` omits the member entirely. Returns the
    /// status and the response body — the two 400 branches of
    /// `sse_messages_handler` are distinguishable only by their body.
    async fn post_sse_message_with_params(
        app: &Router,
        session_id: &str,
        id: i64,
        params: Value,
    ) -> (StatusCode, String) {
        use axum::body::Body;
        use axum::http::Request;
        use http_body_util::BodyExt;
        use tower::ServiceExt;

        let mut body = serde_json::json!({ "jsonrpc": "2.0", "id": id, "method": "test" });
        if !params.is_null() {
            body["params"] = params;
        }
        let req = Request::builder()
            .method("POST")
            .uri(format!("/messages/?sessionId={session_id}"))
            .header("content-type", "application/json")
            .body(Body::from(serde_json::to_vec(&body).unwrap()))
            .unwrap();
        let resp = app.clone().oneshot(req).await.unwrap();
        let status = resp.status();
        let bytes = resp.into_body().collect().await.unwrap().to_bytes();
        (status, String::from_utf8_lossy(&bytes).to_string())
    }

    /// Wait for the consumer task to notice its client left and deregister.
    ///
    /// Polls the registry rather than probing `POST /messages/`: a probe is
    /// itself a message, and the zombie-consumer regression only tears down
    /// *after* pulling one more message off the queue, so a probing wait
    /// would feed the zombie and then report success.
    ///
    /// Panics if the session outlives its stream — that is the registry leak
    /// this suite exists to catch, so it must fail loudly rather than fall
    /// through unasserted.
    async fn await_session_removal(sessions: &SseSessions, session_id: &str) {
        for _ in 0..1000 {
            tokio::task::yield_now().await;
            if !lock_sessions(sessions).contains_key(session_id) {
                return;
            }
        }
        panic!("session {session_id} was still registered after its stream closed");
    }

    fn sse_test_app() -> Router {
        sse_test_app_parts().0
    }

    /// An SSE test app plus its live-session registry.
    fn sse_test_app_parts() -> (Router, SseSessions) {
        sse_test_app_parts_with(Arc::new(TransportManager::new(None)), Arc::new(EchoHandler))
    }

    /// As [`sse_test_app_parts`], with a caller-supplied manager and handler.
    fn sse_test_app_parts_with(
        tm: Arc<TransportManager>,
        handler: Arc<dyn McpHandler>,
    ) -> (Router, SseSessions) {
        #[allow(deprecated)]
        tm.build_sse_app_parts(handler, None)
    }

    #[tokio::test]
    async fn sse_messages_post_returns_202_for_a_live_session() {
        let app = sse_test_app();
        let (_body, session_id) = open_sse_session(&app).await;
        assert_eq!(
            post_sse_message(&app, &session_id, 1).await,
            StatusCode::ACCEPTED
        );
    }

    #[tokio::test]
    async fn sse_messages_post_rejects_missing_session_id() {
        use axum::body::Body;
        use axum::http::Request;
        use tower::ServiceExt;

        let app = sse_test_app();
        let body = serde_json::json!({ "jsonrpc": "2.0", "id": 1, "method": "test" });
        let req = Request::builder()
            .method("POST")
            .uri("/messages/")
            .header("content-type", "application/json")
            .body(Body::from(serde_json::to_vec(&body).unwrap()))
            .unwrap();
        let resp = app.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
    }

    #[tokio::test]
    async fn sse_messages_post_rejects_unknown_session_id() {
        let app = sse_test_app();
        assert_eq!(
            post_sse_message(&app, "no-such-session", 1).await,
            StatusCode::BAD_REQUEST
        );
    }

    #[tokio::test]
    async fn sse_get_returns_event_stream() {
        use axum::body::Body;
        use axum::http::Request;
        use tower::ServiceExt;

        let app = sse_test_app();
        let req = Request::builder().uri("/sse").body(Body::empty()).unwrap();
        let resp = app.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let ct = resp
            .headers()
            .get("content-type")
            .unwrap()
            .to_str()
            .unwrap();
        assert!(ct.contains("text/event-stream"));
    }

    #[tokio::test]
    async fn sse_get_emits_endpoint_event_before_any_message() {
        // A spec-compliant SSE client waits for `endpoint` to learn the POST
        // URL; a stream that stays silent until the first response hangs it.
        let app = sse_test_app();
        let (_body, session_id) = open_sse_session(&app).await;
        assert!(!session_id.is_empty());
    }

    #[tokio::test]
    async fn sse_dispatch_stamps_the_connection_session_id() {
        // Without this the session id never reaches the AsyncTaskBridge:
        // `notify_cancel` fires with the server-minted uuid on disconnect,
        // but `session_tasks` is keyed from `params._meta.sessionId`, so
        // `cancel_session_tasks` matches zero tasks and a submitted task
        // runs to completion after the client is gone.
        let app = sse_test_app();
        let (mut body, session_id) = open_sse_session(&app).await;
        assert_eq!(
            post_sse_message_with_params(&app, &session_id, 1, serde_json::json!({}))
                .await
                .0,
            StatusCode::ACCEPTED
        );

        // EchoHandler mirrors `params` back as `result`.
        let responses = collect_sse_responses(&mut body, 1).await;
        assert_eq!(
            responses[0]["result"]["_meta"]["sessionId"],
            Value::String(session_id.clone()),
            "the dispatched message must carry this connection's session id"
        );
    }

    #[tokio::test]
    async fn sse_dispatch_overwrites_a_client_supplied_session_id() {
        // A client that could name its own session could name someone
        // else's: `notifications/cancelled` routes a caller-supplied key
        // straight into `cancel_session_tasks`. The transport id must win.
        let app = sse_test_app();
        let (mut body, session_id) = open_sse_session(&app).await;
        let params = serde_json::json!({
            "_meta": { "sessionId": "victim-session", "progressToken": 7 }
        });
        assert_eq!(
            post_sse_message_with_params(&app, &session_id, 1, params)
                .await
                .0,
            StatusCode::ACCEPTED
        );

        let responses = collect_sse_responses(&mut body, 1).await;
        let meta = &responses[0]["result"]["_meta"];
        assert_eq!(
            meta["sessionId"],
            Value::String(session_id),
            "a client-supplied sessionId must be overwritten, not honoured"
        );
        assert_eq!(
            meta["progressToken"], 7,
            "stamping must preserve the other _meta members"
        );
    }

    #[test]
    fn stamp_session_id_replaces_a_non_object_meta() {
        let mut message = serde_json::json!({
            "method": "tools/call",
            "params": { "name": "t", "_meta": "not-an-object" }
        });
        stamp_session_id(&mut message, "sess-1");
        assert_eq!(message["params"]["_meta"]["sessionId"], "sess-1");
        assert_eq!(message["params"]["name"], "t");
    }

    #[test]
    fn stamp_session_id_leaves_a_message_without_object_params_untouched() {
        // Nothing to stamp and nothing to smuggle: the handler reads
        // `params._meta` by key, which neither shape can satisfy.
        for mut message in [
            serde_json::json!({ "method": "tools/list" }),
            serde_json::json!({ "method": "tools/list", "params": [1, 2] }),
        ] {
            let before = message.clone();
            stamp_session_id(&mut message, "sess-1");
            assert_eq!(message, before);
        }
    }

    #[tokio::test]
    async fn sse_concurrent_sessions_receive_only_their_own_responses() {
        // Two simultaneous streams. The pre-fix implementation shared one
        // process-global receiver behind an `Arc<Mutex<..>>` whose guard was
        // held across `recv().await`; tokio's Mutex is FIFO-fair, so the two
        // consumers alternated strictly and handed out responses round-robin.
        //
        // The ids are therefore posted in *runs* — 1,2,3 to A then 4,5,6 to B
        // — and asserted unsorted. Round-robin over a shared queue can only
        // produce the alternating split {1,3,5}/{2,4,6}, so it cannot satisfy
        // these assertions no matter how the streams are scheduled. (An
        // interleaved post order plus `sort_unstable()` is satisfied *by* the
        // bug, which is what the previous version of this test did.)
        let (app, sessions) = sse_test_app_parts();
        let (mut body_a, session_a) = open_sse_session(&app).await;
        let (mut body_b, session_b) = open_sse_session(&app).await;
        assert_ne!(
            session_a, session_b,
            "each connection needs its own session"
        );

        for (session, id) in [
            (&session_a, 1),
            (&session_a, 2),
            (&session_a, 3),
            (&session_b, 4),
            (&session_b, 5),
            (&session_b, 6),
        ] {
            assert_eq!(
                post_sse_message(&app, session, id).await,
                StatusCode::ACCEPTED
            );
        }

        let ids_a = collect_sse_response_ids(&mut body_a, 3).await;
        let ids_b = collect_sse_response_ids(&mut body_b, 3).await;
        assert_eq!(
            ids_a,
            vec![1, 2, 3],
            "stream A leaked, lost or reordered responses"
        );
        assert_eq!(
            ids_b,
            vec![4, 5, 6],
            "stream B leaked, lost or reordered responses"
        );

        // Partial regression guard: isolation restored but the zombie
        // consumer reintroduced. A leaves; its consumer must go with it
        // unprompted (so the very next post to A is rejected rather than
        // silently eaten), while B — untouched by A's departure — keeps
        // working.
        drop(body_a);
        await_session_removal(&sessions, &session_a).await;
        let (status, message) =
            post_sse_message_with_params(&app, &session_a, 99, Value::Null).await;
        assert_eq!(
            status,
            StatusCode::BAD_REQUEST,
            "a departed session must stop accepting messages"
        );
        assert_eq!(message, "unknown session");
        assert_eq!(
            post_sse_message(&app, &session_b, 7).await,
            StatusCode::ACCEPTED,
            "one client leaving must not disturb another's session"
        );
        assert_eq!(collect_sse_response_ids(&mut body_b, 1).await, vec![7]);
    }

    #[tokio::test]
    async fn sse_disconnect_does_not_swallow_later_messages() {
        // Every past disconnect used to leave a zombie consumer that ate
        // exactly one subsequent message while POST still answered 202.
        let (app, sessions) = sse_test_app_parts();
        for _ in 0..3 {
            let (body, session_id) = open_sse_session(&app).await;
            drop(body);
            // The consumer must notice the closed stream on its own, without
            // being handed a message to trip over.
            await_session_removal(&sessions, &session_id).await;
            let (status, message) =
                post_sse_message_with_params(&app, &session_id, 0, Value::Null).await;
            assert_eq!(
                status,
                StatusCode::BAD_REQUEST,
                "the first message after a disconnect was consumed, not rejected"
            );
            assert_eq!(message, "unknown session");
        }

        let (mut body, session_id) = open_sse_session(&app).await;
        for id in [1, 2, 3] {
            assert_eq!(
                post_sse_message(&app, &session_id, id).await,
                StatusCode::ACCEPTED
            );
        }
        let ids = collect_sse_response_ids(&mut body, 3).await;
        assert_eq!(
            ids,
            vec![1, 2, 3],
            "messages were swallowed after disconnect"
        );
    }

    #[tokio::test]
    async fn sse_disconnect_deregisters_the_session() {
        // Status codes alone cannot certify deregistration: drop the
        // `sessions.remove(..)` call and a post to a dead session still
        // answers 400, just via the "the stream closed between lookup and
        // send" arm instead of the registry miss. The two are told apart
        // only by their body, and only the registry itself can show that
        // the entry is gone rather than merely inert — an unbounded leak
        // otherwise goes undetected for the lifetime of the process.
        let (app, sessions) = sse_test_app_parts();

        let (body, session_id) = open_sse_session(&app).await;
        assert!(
            lock_sessions(&sessions).contains_key(&session_id),
            "an open stream must be registered"
        );

        drop(body);
        await_session_removal(&sessions, &session_id).await;
        assert!(
            lock_sessions(&sessions).is_empty(),
            "the closed session leaked into the registry"
        );

        let (status, message) =
            post_sse_message_with_params(&app, &session_id, 1, Value::Null).await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
        assert_eq!(
            message, "unknown session",
            "the rejection must come from the registry miss, not from a send \
             onto a dropped receiver of a still-registered session"
        );
    }

    /// A handler that panics on every message, standing in for any bug in
    /// real tool dispatch.
    struct PanickingHandler;

    #[async_trait::async_trait]
    impl McpHandler for PanickingHandler {
        async fn handle_message(&self, _message: Value) -> Option<Value> {
            panic!("deliberate handler panic");
        }
    }

    #[tokio::test]
    async fn sse_session_teardown_survives_a_handler_panic() {
        // Teardown used to be two statements after the dispatch loop, which
        // an unwind walks straight past: the registry entry leaked for the
        // lifetime of the process and TM-4 cancellation never ran for the
        // tasks that session had launched. `SseSessionGuard`'s Drop runs
        // during the unwind instead.
        //
        // The panic backtrace this prints to stderr is expected.
        let cancelled: Arc<std::sync::Mutex<Vec<String>>> = Arc::new(std::sync::Mutex::new(vec![]));
        let recorder = Arc::clone(&cancelled);
        let mut tm = TransportManager::new(None);
        tm.set_cancel_handler(Some(Arc::new(move |session_id: &str| {
            recorder
                .lock()
                .unwrap_or_else(|p| p.into_inner())
                .push(session_id.to_string());
        })));
        let (app, sessions) = sse_test_app_parts_with(Arc::new(tm), Arc::new(PanickingHandler));

        let (_body, session_id) = open_sse_session(&app).await;
        assert_eq!(
            post_sse_message(&app, &session_id, 1).await,
            StatusCode::ACCEPTED
        );

        await_session_removal(&sessions, &session_id).await;
        assert!(
            lock_sessions(&sessions).is_empty(),
            "a panicking handler leaked its session into the registry"
        );
        assert_eq!(
            *cancelled.lock().unwrap_or_else(|p| p.into_inner()),
            vec![session_id],
            "TM-4 cancellation must still fire when the handler panics"
        );
    }

    #[tokio::test]
    async fn sse_extra_routes_are_merged() {
        use axum::body::Body;
        use axum::http::Request;
        use tower::ServiceExt;

        let tm = Arc::new(TransportManager::new(None));
        let handler: Arc<dyn McpHandler> = Arc::new(EchoHandler);
        let extra = Router::new().route("/extra", get(|| async { "extra" }));
        #[allow(deprecated)]
        let app = tm.build_sse_app(handler, Some(extra));

        let req = Request::builder()
            .uri("/extra")
            .body(Body::empty())
            .unwrap();
        let resp = app.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
    }

    #[allow(deprecated)]
    #[tokio::test]
    async fn run_sse_rejects_empty_host() {
        let tm = Arc::new(TransportManager::new(None));
        let handler: Arc<dyn McpHandler> = Arc::new(EchoHandler);
        let result = tm.run_sse(handler, "", 8080, None).await;
        assert!(matches!(result, Err(TransportError::InvalidHost(_))));
    }

    #[allow(deprecated)]
    #[tokio::test]
    async fn run_sse_rejects_port_zero() {
        let tm = Arc::new(TransportManager::new(None));
        let handler: Arc<dyn McpHandler> = Arc::new(EchoHandler);
        let result = tm.run_sse(handler, "localhost", 0, None).await;
        assert!(matches!(result, Err(TransportError::InvalidPort(0))));
    }

    // -----------------------------------------------------------------------
    // Additional unit tests: edge cases and error paths
    // -----------------------------------------------------------------------

    #[test]
    fn transport_error_bind_display() {
        // Construct a Bind error with a hyper::Error source.
        // We can't easily construct hyper::Error directly, so test the other variants.
        let err = TransportError::Server("bind failed".to_string());
        assert!(err.to_string().contains("bind failed"));
    }

    #[test]
    fn transport_error_is_debug() {
        let err = TransportError::InvalidPort(99);
        let debug = format!("{:?}", err);
        assert!(debug.contains("InvalidPort"));
        assert!(debug.contains("99"));
    }

    #[test]
    fn transport_error_invalid_host_preserves_value() {
        let err = TransportError::InvalidHost("bad-host!".to_string());
        assert_eq!(err.to_string(), "invalid host: bad-host!");
    }

    #[test]
    fn transport_error_invalid_port_boundary_65535() {
        let err = TransportError::InvalidPort(65535);
        assert_eq!(
            err.to_string(),
            "port must be between 1 and 65535, got 65535"
        );
    }

    #[test]
    fn validate_host_port_rejects_both_invalid() {
        // Empty host is checked first.
        let result = TransportManager::validate_host_port("", 0);
        assert!(matches!(
            result.unwrap_err(),
            TransportError::InvalidHost(_)
        ));
    }

    #[test]
    fn health_response_serializes_correctly() {
        let tm = TransportManager::new(None);
        let resp = tm.build_health_response();
        let json = serde_json::to_value(&resp).unwrap();
        assert!(json.get("status").is_some());
        assert!(json.get("uptime_seconds").is_some());
        assert!(json.get("module_count").is_some());
    }

    #[tokio::test]
    async fn stdio_whitespace_only_lines_are_skipped() {
        let input = "   \n\t\n{\"jsonrpc\":\"2.0\",\"id\":1,\"method\":\"x\"}\n".to_string();
        let reader = std::io::Cursor::new(input.into_bytes());
        let mut output = Vec::new();

        let tm = TransportManager::new(None);
        let handler = EchoHandler;
        tm.run_stdio_with_io(reader, &mut output, &handler)
            .await
            .unwrap();

        let output_str = String::from_utf8(output).unwrap();
        let lines: Vec<&str> = output_str.trim().split('\n').collect();
        assert_eq!(lines.len(), 1);
        let r: Value = serde_json::from_str(lines[0]).unwrap();
        assert_eq!(r["id"], 1);
    }

    #[tokio::test]
    async fn stdio_response_ends_with_newline() {
        let input = r#"{"jsonrpc":"2.0","id":1,"method":"ping"}"#.to_string() + "\n";
        let reader = std::io::Cursor::new(input.into_bytes());
        let mut output = Vec::new();

        let tm = TransportManager::new(None);
        let handler = EchoHandler;
        tm.run_stdio_with_io(reader, &mut output, &handler)
            .await
            .unwrap();

        // Each response line must end with \n.
        assert!(output.last() == Some(&b'\n'));
    }

    #[tokio::test]
    async fn stdio_mixed_notifications_and_requests() {
        let input = concat!(
            r#"{"jsonrpc":"2.0","method":"notify1"}"#,
            "\n",
            r#"{"jsonrpc":"2.0","id":1,"method":"req1"}"#,
            "\n",
            r#"{"jsonrpc":"2.0","method":"notify2"}"#,
            "\n",
            r#"{"jsonrpc":"2.0","id":2,"method":"req2"}"#,
            "\n",
        )
        .to_string();
        let reader = std::io::Cursor::new(input.into_bytes());
        let mut output = Vec::new();

        let tm = TransportManager::new(None);
        let handler = EchoHandler;
        tm.run_stdio_with_io(reader, &mut output, &handler)
            .await
            .unwrap();

        let output_str = String::from_utf8(output).unwrap();
        let lines: Vec<&str> = output_str.trim().split('\n').collect();
        // Only 2 responses for the 2 requests (notifications produce nothing).
        assert_eq!(lines.len(), 2);
    }

    #[tokio::test]
    async fn streamable_http_metrics_returns_200_with_exporter() {
        use axum::body::Body;
        use axum::http::Request;
        use tower::ServiceExt;

        struct MockExporter;
        impl MetricsExporter for MockExporter {
            fn export_prometheus(&self) -> String {
                "test_metric 1\n".to_string()
            }
        }

        let exporter: Arc<dyn MetricsExporter> = Arc::new(MockExporter);
        let tm = Arc::new(TransportManager::new(Some(exporter)));
        let handler: Arc<dyn McpHandler> = Arc::new(EchoHandler);
        let app = tm.build_streamable_http_app(handler, None);

        let req = Request::builder()
            .uri("/metrics")
            .body(Body::empty())
            .unwrap();
        let resp = app.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::OK);

        let body = axum::body::to_bytes(resp.into_body(), usize::MAX)
            .await
            .unwrap();
        assert_eq!(String::from_utf8(body.to_vec()).unwrap(), "test_metric 1\n");
    }

    #[tokio::test]
    async fn streamable_http_unknown_route_returns_404() {
        use axum::body::Body;
        use axum::http::Request;
        use tower::ServiceExt;

        let tm = Arc::new(TransportManager::new(None));
        let handler: Arc<dyn McpHandler> = Arc::new(EchoHandler);
        let app = tm.build_streamable_http_app(handler, None);

        let req = Request::builder()
            .uri("/nonexistent")
            .body(Body::empty())
            .unwrap();
        let resp = app.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::NOT_FOUND);
    }

    #[tokio::test]
    async fn streamable_http_post_mcp_returns_session_id_header() {
        use axum::body::Body;
        use axum::http::Request;
        use tower::ServiceExt;

        let tm = Arc::new(TransportManager::new(None));
        let handler: Arc<dyn McpHandler> = Arc::new(EchoHandler);
        let app = tm.build_streamable_http_app(handler, None);

        let body = serde_json::json!({"jsonrpc":"2.0","id":1,"method":"test"});
        let req = Request::builder()
            .method("POST")
            .uri("/mcp")
            .header("content-type", "application/json")
            .body(Body::from(serde_json::to_vec(&body).unwrap()))
            .unwrap();
        let resp = app.oneshot(req).await.unwrap();

        let session_id = resp
            .headers()
            .get("mcp-session-id")
            .unwrap()
            .to_str()
            .unwrap();
        // Session ID should be a valid UUID (36 chars with hyphens).
        assert_eq!(session_id.len(), 36);
        assert!(uuid::Uuid::parse_str(session_id).is_ok());
    }

    // -----------------------------------------------------------------------
    // Integration-style tests: real TCP listener on ephemeral port
    // -----------------------------------------------------------------------

    /// Helper: collect a hyper Incoming body into bytes.
    async fn collect_body(body: hyper::body::Incoming) -> Vec<u8> {
        use http_body_util::BodyExt;
        let collected = body.collect().await.unwrap();
        collected.to_bytes().to_vec()
    }

    /// Helper: build an HTTP client for integration tests.
    fn test_client() -> hyper_util::client::legacy::Client<
        hyper_util::client::legacy::connect::HttpConnector,
        axum::body::Body,
    > {
        hyper_util::client::legacy::Client::builder(hyper_util::rt::TokioExecutor::new())
            .build_http()
    }

    #[tokio::test]
    async fn integration_health_endpoint_responds() {
        let tm = Arc::new(TransportManager::new(None));
        let handler: Arc<dyn McpHandler> = Arc::new(EchoHandler);
        let app = tm.build_streamable_http_app(handler, None);

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();

        let server = tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });

        tokio::time::sleep(std::time::Duration::from_millis(50)).await;

        let client = test_client();
        let resp = client
            .request(
                hyper::Request::builder()
                    .uri(format!("http://{}/health", addr))
                    .body(axum::body::Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(resp.status(), StatusCode::OK);
        let body = collect_body(resp.into_body()).await;
        let json: Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(json["status"], "ok");
        assert_eq!(json["module_count"], 0);

        server.abort();
    }

    #[tokio::test]
    async fn integration_metrics_endpoint_404_without_exporter() {
        let tm = Arc::new(TransportManager::new(None));
        let handler: Arc<dyn McpHandler> = Arc::new(EchoHandler);
        let app = tm.build_streamable_http_app(handler, None);

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();

        let server = tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });

        tokio::time::sleep(std::time::Duration::from_millis(50)).await;

        let client = test_client();
        let resp = client
            .request(
                hyper::Request::builder()
                    .uri(format!("http://{}/metrics", addr))
                    .body(axum::body::Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(resp.status(), StatusCode::NOT_FOUND);

        server.abort();
    }

    #[tokio::test]
    async fn integration_metrics_endpoint_200_with_exporter() {
        struct TestExporter;
        impl MetricsExporter for TestExporter {
            fn export_prometheus(&self) -> String {
                "# HELP test\ntest_total 99\n".to_string()
            }
        }

        let exporter: Arc<dyn MetricsExporter> = Arc::new(TestExporter);
        let tm = Arc::new(TransportManager::new(Some(exporter)));
        let handler: Arc<dyn McpHandler> = Arc::new(EchoHandler);
        let app = tm.build_streamable_http_app(handler, None);

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();

        let server = tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });

        tokio::time::sleep(std::time::Duration::from_millis(50)).await;

        let client = test_client();
        let resp = client
            .request(
                hyper::Request::builder()
                    .uri(format!("http://{}/metrics", addr))
                    .body(axum::body::Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(resp.status(), StatusCode::OK);
        let body = collect_body(resp.into_body()).await;
        assert_eq!(
            String::from_utf8(body).unwrap(),
            "# HELP test\ntest_total 99\n"
        );

        server.abort();
    }

    #[tokio::test]
    async fn integration_usage_endpoint_404_without_exporter() {
        let tm = Arc::new(TransportManager::new(None));
        let handler: Arc<dyn McpHandler> = Arc::new(EchoHandler);
        let app = tm.build_streamable_http_app(handler, None);
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        let client = test_client();
        let resp = client
            .request(
                hyper::Request::builder()
                    .uri(format!("http://{}/usage", addr))
                    .body(axum::body::Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::NOT_FOUND);
        server.abort();
    }

    #[tokio::test]
    async fn integration_usage_endpoint_200_with_exporter() {
        struct TestUsage;
        impl UsageExporter for TestUsage {
            fn export_json(&self) -> Value {
                serde_json::json!({"modules": [{"module_id": "a", "call_count": 1}]})
            }
        }
        let mut tm = TransportManager::new(None);
        let exporter: Arc<dyn UsageExporter> = Arc::new(TestUsage);
        tm.set_usage_exporter(Some(exporter));
        let tm = Arc::new(tm);
        let handler: Arc<dyn McpHandler> = Arc::new(EchoHandler);
        let app = tm.build_streamable_http_app(handler, None);
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        let client = test_client();
        let resp = client
            .request(
                hyper::Request::builder()
                    .uri(format!("http://{}/usage", addr))
                    .body(axum::body::Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let body = collect_body(resp.into_body()).await;
        let parsed: Value = serde_json::from_slice(&body).unwrap();
        assert!(parsed.get("modules").is_some());
        server.abort();
    }

    #[test]
    fn cancel_handler_invocation() {
        use std::sync::atomic::{AtomicUsize, Ordering};
        let counter = Arc::new(AtomicUsize::new(0));
        let c = Arc::clone(&counter);
        let mut tm = TransportManager::new(None);
        tm.set_cancel_handler(Some(Arc::new(move |_sid: &str| {
            c.fetch_add(1, Ordering::SeqCst);
        })));
        tm.notify_cancel("sess-1");
        tm.notify_cancel("sess-2");
        assert_eq!(counter.load(Ordering::SeqCst), 2);
    }

    #[tokio::test]
    async fn integration_mcp_post_endpoint_responds() {
        let tm = Arc::new(TransportManager::new(None));
        let handler: Arc<dyn McpHandler> = Arc::new(EchoHandler);
        let app = tm.build_streamable_http_app(handler, None);

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();

        let server = tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });

        tokio::time::sleep(std::time::Duration::from_millis(50)).await;

        let body = serde_json::json!({"jsonrpc":"2.0","id":1,"method":"initialize","params":{}});
        let client = test_client();
        let resp = client
            .request(
                hyper::Request::builder()
                    .method("POST")
                    .uri(format!("http://{}/mcp", addr))
                    .header("content-type", "application/json")
                    .body(axum::body::Body::from(serde_json::to_vec(&body).unwrap()))
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(resp.status(), StatusCode::OK);
        let resp_body = collect_body(resp.into_body()).await;
        let json: Value = serde_json::from_slice(&resp_body).unwrap();
        assert_eq!(json["jsonrpc"], "2.0");
        assert_eq!(json["id"], 1);

        server.abort();
    }

    #[tokio::test]
    async fn integration_extra_routes_served() {
        let tm = Arc::new(TransportManager::new(None));
        let handler: Arc<dyn McpHandler> = Arc::new(EchoHandler);
        let extra = Router::new().route("/custom", get(|| async { "hello-integration" }));
        let app = tm.build_streamable_http_app(handler, Some(extra));

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();

        let server = tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });

        tokio::time::sleep(std::time::Duration::from_millis(50)).await;

        let client = test_client();
        let resp = client
            .request(
                hyper::Request::builder()
                    .uri(format!("http://{}/custom", addr))
                    .body(axum::body::Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(resp.status(), StatusCode::OK);
        let body = collect_body(resp.into_body()).await;
        assert_eq!(String::from_utf8(body).unwrap(), "hello-integration");

        server.abort();
    }

    // ---- HttpAuthConfig tests ────────────────────────────────────────

    #[test]
    fn http_auth_config_default() {
        let cfg = HttpAuthConfig::default();
        assert!(cfg.authenticator.is_none());
        assert!(!cfg.require_auth);
        assert!(cfg.explorer_prefix.is_none());
        assert!(cfg.exempt_paths.is_none());
    }

    #[test]
    fn http_auth_config_debug_does_not_panic() {
        let cfg = HttpAuthConfig::default();
        let debug = format!("{:?}", cfg);
        assert!(debug.contains("HttpAuthConfig"));
    }

    #[test]
    fn http_auth_config_clone() {
        let cfg = HttpAuthConfig {
            require_auth: true,
            explorer_prefix: Some("/explorer".into()),
            ..Default::default()
        };
        let cloned = cfg.clone();
        assert!(cloned.require_auth);
        assert_eq!(cloned.explorer_prefix.as_deref(), Some("/explorer"));
    }

    #[test]
    fn apply_auth_layer_without_authenticator_is_noop() {
        let app = Router::new().route("/test", get(|| async { "ok" }));
        let result = super::apply_auth_layer(app, HttpAuthConfig::default());
        // Should return a valid router (no panic).
        let _ = result;
    }
}
