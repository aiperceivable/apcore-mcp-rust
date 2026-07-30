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
        let sse_state = SseState {
            handler,
            sessions: Arc::new(tokio::sync::Mutex::new(std::collections::HashMap::new())),
            manager: Arc::clone(self),
        };

        let sse_router = Router::new()
            .route("/sse", get(sse_stream_handler))
            .route("/messages/", post(sse_messages_handler))
            .with_state(sse_state);

        let mut app = self.health_metrics_router().merge(sse_router);

        if let Some(extra) = extra_routes {
            app = app.merge(extra);
        }

        app
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
type SseSessions =
    Arc<tokio::sync::Mutex<std::collections::HashMap<String, tokio::sync::mpsc::Sender<Value>>>>;

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

/// GET /sse — open a session-scoped server-sent event stream.
///
/// Emits the MCP `endpoint` event carrying this session's POST URL, then
/// streams the responses to messages posted for this session and nothing else.
/// A keep-alive comment is sent periodically so the stream is never silent and
/// intermediaries do not close an idle connection. When the client disconnects
/// the session is removed from the registry and its consumer task exits, so no
/// zombie consumer is left behind to swallow a later message.
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

    state
        .sessions
        .lock()
        .await
        .insert(session_id.clone(), inbound_tx);

    let handler = state.handler.clone();
    let sessions = state.sessions.clone();
    let manager = state.manager.clone();
    let owned_session_id = session_id.clone();

    tokio::spawn(async move {
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
                        let Some(message) = inbound else { break };
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
        sessions.lock().await.remove(&owned_session_id);
        // [TM-4] Cancel any async tasks bound to the disconnecting session,
        // matching the TypeScript `transport.onclose` handler.
        manager.notify_cancel(&owned_session_id);
        tracing::debug!(session_id = %owned_session_id, "sse session closed");
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

    let sender = state.sessions.lock().await.get(&session_id).cloned();
    let Some(sender) = sender else {
        return (StatusCode::BAD_REQUEST, "unknown session").into_response();
    };

    match sender.send(body).await {
        Ok(()) => StatusCode::ACCEPTED.into_response(),
        // The stream closed between lookup and send.
        Err(_) => (StatusCode::BAD_REQUEST, "unknown session").into_response(),
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

    /// Collect the JSON-RPC ids carried by the next `count` `message` events.
    async fn collect_sse_response_ids(body: &mut axum::body::Body, count: usize) -> Vec<i64> {
        let mut ids = Vec::new();
        while ids.len() < count {
            let chunk = read_sse_chunk(body).await;
            for line in chunk.lines() {
                let Some(payload) = line.strip_prefix("data:") else {
                    continue;
                };
                let Ok(value) = serde_json::from_str::<Value>(payload.trim()) else {
                    continue;
                };
                if let Some(id) = value.get("id").and_then(Value::as_i64) {
                    ids.push(id);
                }
            }
        }
        ids
    }

    /// POST a JSON-RPC request bound to a specific SSE session.
    async fn post_sse_message(app: &Router, session_id: &str, id: i64) -> StatusCode {
        use axum::body::Body;
        use axum::http::Request;
        use tower::ServiceExt;

        let body = serde_json::json!({ "jsonrpc": "2.0", "id": id, "method": "test" });
        let req = Request::builder()
            .method("POST")
            .uri(format!("/messages/?sessionId={session_id}"))
            .header("content-type", "application/json")
            .body(Body::from(serde_json::to_vec(&body).unwrap()))
            .unwrap();
        app.clone().oneshot(req).await.unwrap().status()
    }

    fn sse_test_app() -> Router {
        let tm = Arc::new(TransportManager::new(None));
        let handler: Arc<dyn McpHandler> = Arc::new(EchoHandler);
        #[allow(deprecated)]
        tm.build_sse_app(handler, None)
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
    async fn sse_concurrent_sessions_receive_only_their_own_responses() {
        // Two simultaneous streams, messages interleaved across both: each
        // stream must see exactly its own ids. The previous implementation
        // shared one process-global receiver, so responses were handed out
        // round-robin and one client received another client's tool output.
        let app = sse_test_app();
        let (mut body_a, session_a) = open_sse_session(&app).await;
        let (mut body_b, session_b) = open_sse_session(&app).await;
        assert_ne!(
            session_a, session_b,
            "each connection needs its own session"
        );

        for (session, id) in [
            (&session_a, 1),
            (&session_b, 2),
            (&session_a, 3),
            (&session_b, 4),
            (&session_a, 5),
            (&session_b, 6),
        ] {
            assert_eq!(
                post_sse_message(&app, session, id).await,
                StatusCode::ACCEPTED
            );
        }

        let mut ids_a = collect_sse_response_ids(&mut body_a, 3).await;
        let mut ids_b = collect_sse_response_ids(&mut body_b, 3).await;
        ids_a.sort_unstable();
        ids_b.sort_unstable();
        assert_eq!(ids_a, vec![1, 3, 5], "stream A leaked or lost responses");
        assert_eq!(ids_b, vec![2, 4, 6], "stream B leaked or lost responses");
    }

    #[tokio::test]
    async fn sse_disconnect_does_not_swallow_later_messages() {
        // Every past disconnect used to leave a zombie consumer that ate
        // exactly one subsequent message while POST still answered 202.
        let app = sse_test_app();
        for _ in 0..3 {
            let (body, session_id) = open_sse_session(&app).await;
            drop(body);
            // Let the consumer task observe the closed stream and deregister.
            for _ in 0..50 {
                tokio::task::yield_now().await;
                if post_sse_message(&app, &session_id, 0).await == StatusCode::BAD_REQUEST {
                    break;
                }
            }
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
