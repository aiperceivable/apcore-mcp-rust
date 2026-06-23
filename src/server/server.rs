//! MCPServer — the main MCP server struct.
//!
//! Combines the factory, router, transport, and listener into a single
//! server lifecycle.

use std::collections::HashSet;
use std::fmt;
use std::future::Future;
use std::pin::Pin;
use std::str::FromStr;
use std::sync::Arc;

use serde_json::Value;
use tokio::sync::watch;
use tokio::task::JoinHandle;

use crate::server::types::{
    CallToolResult, InitializationOptions, ReadResourceContents, Resource, Tool,
};

// ---------------------------------------------------------------------------
// TransportKind
// ---------------------------------------------------------------------------

/// The transport protocol used by the MCP server.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TransportKind {
    /// Standard I/O transport (stdin/stdout).
    Stdio,
    /// Streamable HTTP transport.
    StreamableHttp,
    /// Server-Sent Events transport.
    Sse,
}

/// Error returned when parsing an invalid transport string.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("unknown transport: \"{0}\"")]
pub struct ParseTransportError(String);

impl FromStr for TransportKind {
    type Err = ParseTransportError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s.to_ascii_lowercase().as_str() {
            "stdio" => Ok(Self::Stdio),
            "streamable-http" => Ok(Self::StreamableHttp),
            "sse" => Ok(Self::Sse),
            _ => Err(ParseTransportError(s.to_string())),
        }
    }
}

impl fmt::Display for TransportKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Stdio => write!(f, "stdio"),
            Self::StreamableHttp => write!(f, "streamable-http"),
            Self::Sse => write!(f, "sse"),
        }
    }
}

impl TransportKind {
    /// Return the address string for this transport.
    ///
    /// * `Stdio` always returns `"stdio"` (host/port are ignored).
    /// * `StreamableHttp` and `Sse` return `"http://{host}:{port}"`.
    pub fn address(&self, host: &str, port: u16) -> String {
        match self {
            Self::Stdio => "stdio".to_string(),
            Self::StreamableHttp | Self::Sse => format!("http://{}:{}", host, port),
        }
    }
}

// ---------------------------------------------------------------------------
// MCPServerConfig
// ---------------------------------------------------------------------------

/// Configuration for constructing an [`MCPServer`].
///
/// Defaults match the Python `MCPServer.__init__` defaults.
#[derive(Clone)]
pub struct MCPServerConfig {
    /// Transport protocol.
    pub transport: TransportKind,
    /// Bind host for network transports.
    pub host: String,
    /// Bind port for network transports.
    pub port: u16,
    /// Server name advertised in MCP init.
    pub name: String,
    /// Server version string.
    pub version: Option<String>,
    /// Whether to validate tool inputs against their JSON schema.
    pub validate_inputs: bool,
    /// Optional tags to filter which modules are exposed as tools.
    pub tags: Option<Vec<String>>,
    /// Optional prefix to filter which modules are exposed as tools.
    pub prefix: Option<String>,
    /// Whether authentication is required for HTTP transports.
    pub require_auth: bool,
    /// Paths exempt from authentication.
    pub exempt_paths: Option<HashSet<String>>,
    // ---- [M-1] Config-surface alignment with the builder -------------------
    /// Enable pipeline trace mode (responses include strategy/duration/steps).
    pub trace: bool,
    /// Pipeline execution strategy preset (e.g. "standard", "internal").
    pub strategy: Option<String>,
    /// Enable the browser-based Tool Explorer UI (HTTP transports).
    pub explorer: bool,
    /// URL prefix for the explorer.
    pub explorer_prefix: String,
    /// Page title shown in the explorer browser tab and heading.
    pub explorer_title: String,
    /// Optional project name shown in the explorer footer (branding).
    pub explorer_project_name: Option<String>,
    /// Optional project URL linked in the explorer footer (branding).
    pub explorer_project_url: Option<String>,
    /// Allow tool execution from the explorer UI.
    pub allow_execute: bool,
    /// Optional custom output formatter closure (shareable/cloneable variant).
    pub output_formatter: Option<crate::server::router::SharedOutputFormatter>,
    // NOTE: authenticator and metrics_collector are trait-object fields.
    // They will be added when their trait definitions are available.
    // pub authenticator: Option<Arc<dyn Authenticator>>,
    // pub metrics_collector: Option<Arc<dyn MetricsExporter>>,
}

impl fmt::Debug for MCPServerConfig {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("MCPServerConfig")
            .field("transport", &self.transport)
            .field("host", &self.host)
            .field("port", &self.port)
            .field("name", &self.name)
            .field("version", &self.version)
            .field("validate_inputs", &self.validate_inputs)
            .field("tags", &self.tags)
            .field("prefix", &self.prefix)
            .field("require_auth", &self.require_auth)
            .field("exempt_paths", &self.exempt_paths)
            .field("trace", &self.trace)
            .field("strategy", &self.strategy)
            .field("explorer", &self.explorer)
            .field("explorer_prefix", &self.explorer_prefix)
            .field("explorer_title", &self.explorer_title)
            .field("explorer_project_name", &self.explorer_project_name)
            .field("explorer_project_url", &self.explorer_project_url)
            .field("allow_execute", &self.allow_execute)
            .field(
                "output_formatter",
                &self.output_formatter.as_ref().map(|_| "<closure>"),
            )
            .finish()
    }
}

impl Default for MCPServerConfig {
    fn default() -> Self {
        Self {
            transport: TransportKind::Stdio,
            host: "127.0.0.1".to_string(),
            port: 8000,
            name: "apcore-mcp".to_string(),
            version: None,
            validate_inputs: false,
            tags: None,
            prefix: None,
            require_auth: true,
            exempt_paths: None,
            trace: false,
            strategy: None,
            explorer: false,
            explorer_prefix: "/explorer".to_string(),
            explorer_title: "MCP Tool Explorer".to_string(),
            explorer_project_name: None,
            explorer_project_url: None,
            allow_execute: false,
            output_formatter: None,
        }
    }
}

// ---------------------------------------------------------------------------
// FactoryError
// ---------------------------------------------------------------------------

/// Error type for factory/handler operations.
#[derive(Debug, thiserror::Error)]
pub enum FactoryError {
    #[error("Resource not found: {0}")]
    ResourceNotFound(String),
    #[error("Unsupported URI scheme: {0}")]
    UnsupportedScheme(String),
    /// A user module's id collides with the reserved `__apcore_` namespace
    /// owned by the AsyncTaskBridge meta-tools. This is a fatal startup
    /// configuration error — Python and TypeScript both raise/throw on
    /// this case and crash the server. [A-D-010]
    #[error(
        "Reserved module id: '{0}' (the '__apcore_' prefix is owned by the async task bridge)"
    )]
    ReservedPrefix(String),
    /// The MCP server name supplied to
    /// [`MCPServerFactory::create_server`](super::factory::MCPServerFactory::create_server)
    /// failed validation. The protocol spec requires the name to be a
    /// non-empty string of at most 255 characters. The payload is the
    /// length of the offending name (0 for empty). [D10-002]
    #[error("Invalid server name: length {0} (must be non-empty and at most 255 characters)")]
    InvalidName(usize),
    #[error("{0}")]
    Other(String),
}

// ---------------------------------------------------------------------------
// CallToolHandler
// ---------------------------------------------------------------------------

/// Type alias for the async call_tool handler.
pub type CallToolHandler = Arc<
    dyn Fn(String, Value, Option<Value>) -> Pin<Box<dyn Future<Output = CallToolResult> + Send>>
        + Send
        + Sync,
>;

/// Type alias for the read_resource handler.
pub type ReadResourceHandler =
    Arc<dyn Fn(String) -> Result<Vec<ReadResourceContents>, FactoryError> + Send + Sync>;

// ---------------------------------------------------------------------------
// RegistryOrExecutor
// ---------------------------------------------------------------------------

/// Input to [`MCPServer`]: either a registry or an executor.
///
/// Holds the concrete apcore types. `apcore::Executor`, `apcore::Registry`,
/// and `apcore::Module` are all `pub` in the `apcore` crate (and used
/// directly by the parent `apcore_mcp` module), so the server can depend on
/// them without an `Any` indirection.
#[derive(Clone)]
pub enum RegistryOrExecutor {
    /// A standalone apcore [`Registry`](apcore::registry::registry::Registry).
    Registry(Arc<apcore::registry::registry::Registry>),
    /// A standalone apcore [`Executor`](apcore::executor::Executor), which
    /// owns its own registry.
    Executor(Arc<apcore::executor::Executor>),
}

impl fmt::Debug for RegistryOrExecutor {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Registry(_) => f.write_str("RegistryOrExecutor::Registry(..)"),
            Self::Executor(_) => f.write_str("RegistryOrExecutor::Executor(..)"),
        }
    }
}

// ---------------------------------------------------------------------------
// MCPServer
// ---------------------------------------------------------------------------

/// The MCP server. Created by [`MCPServerFactory`](super::factory::MCPServerFactory).
pub struct MCPServer {
    config: MCPServerConfig,

    /// Optional registry-or-executor input. Retained so callers that
    /// construct via [`MCPServer::with_registry_or_executor`] can introspect
    /// the backend; handler wiring is performed by
    /// [`MCPServerFactory`](super::factory::MCPServerFactory).
    registry_or_executor: Option<RegistryOrExecutor>,

    // --- Handler storage ---
    /// Handler for `list_tools` requests.
    pub(crate) list_tools_handler: Option<Arc<dyn Fn() -> Vec<Tool> + Send + Sync>>,
    /// Handler for `call_tool` requests.
    pub(crate) call_tool_handler: Option<CallToolHandler>,
    /// Handler for `list_resources` requests.
    pub(crate) list_resources_handler: Option<Arc<dyn Fn() -> Vec<Resource> + Send + Sync>>,
    /// Handler for `read_resource` requests.
    pub(crate) read_resource_handler: Option<ReadResourceHandler>,

    // --- Lifecycle state ---
    /// Handle for the spawned server task.
    join_handle: Option<JoinHandle<Result<(), Box<dyn std::error::Error + Send + Sync>>>>,
    /// Sender side of the shutdown watch channel.
    shutdown_tx: Option<watch::Sender<bool>>,
}

impl MCPServer {
    /// Create a new MCP server from a configuration.
    pub fn new(config: MCPServerConfig) -> Self {
        Self {
            config,
            registry_or_executor: None,
            list_tools_handler: None,
            call_tool_handler: None,
            list_resources_handler: None,
            read_resource_handler: None,
            join_handle: None,
            shutdown_tx: None,
        }
    }

    /// Create a new MCP server from a [`RegistryOrExecutor`] and configuration.
    pub fn with_registry_or_executor(
        registry_or_executor: RegistryOrExecutor,
        config: MCPServerConfig,
    ) -> Self {
        Self {
            config,
            registry_or_executor: Some(registry_or_executor),
            list_tools_handler: None,
            call_tool_handler: None,
            list_resources_handler: None,
            read_resource_handler: None,
            join_handle: None,
            shutdown_tx: None,
        }
    }

    /// Create a new MCP server with individual parameters (legacy API).
    ///
    /// Prefer [`MCPServer::new`] with [`MCPServerConfig`] for new code.
    pub fn with_params(name: &str, transport: &str, host: &str, port: u16) -> Self {
        let transport_kind = transport
            .parse::<TransportKind>()
            .unwrap_or(TransportKind::Stdio);
        let config = MCPServerConfig {
            name: name.to_string(),
            transport: transport_kind,
            host: host.to_string(),
            port,
            ..Default::default()
        };
        Self::new(config)
    }

    /// Returns the server name.
    pub fn name(&self) -> &str {
        &self.config.name
    }

    /// Returns the transport kind.
    pub fn transport(&self) -> TransportKind {
        self.config.transport
    }

    /// Returns a reference to the server configuration.
    pub fn config(&self) -> &MCPServerConfig {
        &self.config
    }

    /// Returns true if tool handlers have been registered.
    pub fn has_tool_handlers(&self) -> bool {
        self.list_tools_handler.is_some() && self.call_tool_handler.is_some()
    }

    /// Returns true if resource handlers have been registered.
    pub fn has_resource_handlers(&self) -> bool {
        self.list_resources_handler.is_some() && self.read_resource_handler.is_some()
    }

    /// Invoke the list_tools handler if registered.
    pub fn list_tools(&self) -> Option<Vec<Tool>> {
        self.list_tools_handler.as_ref().map(|h| h())
    }

    /// Invoke the call_tool handler if registered.
    pub fn call_tool(
        &self,
        name: String,
        arguments: Value,
        extra: Option<Value>,
    ) -> Option<Pin<Box<dyn Future<Output = CallToolResult> + Send>>> {
        self.call_tool_handler
            .as_ref()
            .map(|h| h(name, arguments, extra))
    }

    /// Invoke the list_resources handler if registered.
    pub fn list_resources(&self) -> Option<Vec<Resource>> {
        self.list_resources_handler.as_ref().map(|h| h())
    }

    /// Invoke the read_resource handler if registered.
    pub fn read_resource(
        &self,
        uri: String,
    ) -> Option<Result<Vec<ReadResourceContents>, FactoryError>> {
        self.read_resource_handler.as_ref().map(|h| h(uri))
    }

    /// Returns true if the server task is currently running.
    pub fn is_running(&self) -> bool {
        self.join_handle.is_some()
    }

    /// Returns a reference to the registry-or-executor, if one was provided.
    pub fn registry_or_executor(&self) -> Option<&RegistryOrExecutor> {
        self.registry_or_executor.as_ref()
    }

    /// Start the server (spawns the transport loop).
    ///
    /// This is idempotent: calling `start()` on an already-running server is
    /// a no-op and returns `Ok(())`.
    ///
    /// When tool handlers have been registered (via
    /// [`MCPServerFactory::register_handlers`](super::factory::MCPServerFactory::register_handlers))
    /// the spawned task drives the configured transport
    /// ([`TransportManager`](super::transport::TransportManager)) — stdio,
    /// streamable-HTTP, or SSE — racing the transport future against the
    /// shutdown channel. When no handlers are registered the task simply
    /// awaits shutdown (used by lifecycle tests that exercise start/stop
    /// without a real transport).
    pub async fn start(&mut self) -> Result<(), Box<dyn std::error::Error>> {
        // Idempotent: already started.
        if self.join_handle.is_some() {
            return Ok(());
        }

        let (shutdown_tx, mut shutdown_rx) = watch::channel(false);
        self.shutdown_tx = Some(shutdown_tx);

        let (started_tx, started_rx) = tokio::sync::oneshot::channel::<()>();

        // Build a transport handler from the registered tool handlers, if any.
        let init_options = InitializationOptions {
            server_name: self.config.name.clone(),
            server_version: self
                .config
                .version
                .clone()
                .unwrap_or_else(|| crate::VERSION.to_string()),
            capabilities: crate::server::types::ServerCapabilities {
                tools: if self.has_tool_handlers() {
                    Some(crate::server::types::ToolsCapability { list_changed: true })
                } else {
                    None
                },
                resources: if self.has_resource_handlers() {
                    Some(crate::server::types::ResourcesCapability { list_changed: true })
                } else {
                    None
                },
            },
        };
        let handler: Option<Arc<dyn crate::server::transport::McpHandler>> =
            ServerHandler::from_server(self, init_options)
                .map(|h| Arc::new(h) as Arc<dyn crate::server::transport::McpHandler>);

        let transport = self.config.transport;
        let host = self.config.host.clone();
        let port = self.config.port;

        let handle: JoinHandle<Result<(), Box<dyn std::error::Error + Send + Sync>>> =
            tokio::spawn(async move {
                // Signal that the task has started.
                let _ = started_tx.send(());

                match handler {
                    Some(handler) => {
                        use crate::server::transport::TransportManager;
                        let tm = Arc::new(TransportManager::new(None));
                        // Race the transport future against the shutdown signal
                        // so `stop()` cleanly tears the server down.
                        tokio::select! {
                            res = Self::run_transport(&tm, handler, transport, &host, port) => {
                                res.map_err(|e| -> Box<dyn std::error::Error + Send + Sync> {
                                    Box::new(e)
                                })
                            }
                            _ = shutdown_rx.changed() => Ok(()),
                        }
                    }
                    None => {
                        // No handlers registered — just await shutdown.
                        let _ = shutdown_rx.changed().await;
                        Ok(())
                    }
                }
            });

        self.join_handle = Some(handle);

        // Wait for the spawned task to signal it has started (with timeout).
        tokio::time::timeout(std::time::Duration::from_secs(10), started_rx)
            .await
            .map_err(|_| "server start timed out")?
            .map_err(|_| "server start channel dropped")?;

        Ok(())
    }

    /// Drive the configured transport with the given MCP handler. Mirrors the
    /// transport selection in
    /// [`APCoreMCP::serve_with_options`](crate::apcore_mcp::APCoreMCP::serve_with_options).
    async fn run_transport(
        tm: &Arc<crate::server::transport::TransportManager>,
        handler: Arc<dyn crate::server::transport::McpHandler>,
        transport: TransportKind,
        host: &str,
        port: u16,
    ) -> Result<(), crate::server::transport::TransportError> {
        match transport {
            TransportKind::Stdio => tm.run_stdio(&*handler).await,
            TransportKind::StreamableHttp => {
                tm.run_streamable_http(handler, host, port, None).await
            }
            #[allow(deprecated)]
            TransportKind::Sse => tm.run_sse(handler, host, port, None).await,
        }
    }

    /// Wait for the server to shut down.
    ///
    /// If the server has not been started, this returns immediately.
    pub async fn wait(&mut self) -> Result<(), Box<dyn std::error::Error>> {
        if let Some(handle) = self.join_handle.take() {
            handle
                .await
                .map_err(|e| -> Box<dyn std::error::Error> { Box::new(e) })?
                .map_err(|e| -> Box<dyn std::error::Error> { e })?;
        }
        Ok(())
    }

    /// Gracefully stop the server.
    ///
    /// If the server has not been started, this is a no-op.
    pub async fn stop(&mut self) -> Result<(), Box<dyn std::error::Error>> {
        if let Some(tx) = &self.shutdown_tx {
            let _ = tx.send(true);
        }
        // Wait for the spawned task to finish.
        if let Some(handle) = self.join_handle.take() {
            handle
                .await
                .map_err(|e| -> Box<dyn std::error::Error> { Box::new(e) })?
                .map_err(|e| -> Box<dyn std::error::Error> { e })?;
        }
        self.shutdown_tx = None;
        Ok(())
    }

    /// The address the server is listening on.
    ///
    /// Delegates to [`TransportKind::address`].
    pub fn address(&self) -> String {
        self.config
            .transport
            .address(&self.config.host, self.config.port)
    }
}

// ---------------------------------------------------------------------------
// McpHandler implementation — bridges MCPServer handlers to the transport layer
// ---------------------------------------------------------------------------

/// Wraps the MCPServer's tool handlers so the transport layer can dispatch
/// JSON-RPC messages through them.
pub struct ServerHandler {
    list_tools: Arc<dyn Fn() -> Vec<Tool> + Send + Sync>,
    call_tool: CallToolHandler,
    list_resources: Option<Arc<dyn Fn() -> Vec<Resource> + Send + Sync>>,
    read_resource: Option<ReadResourceHandler>,
    init_options: InitializationOptions,
    /// Optional cancel handler invoked on `notifications/cancelled` with
    /// the session id (or request id, serialised). Used to forward
    /// cancellation to the [`AsyncTaskBridge`].
    #[allow(clippy::type_complexity)]
    cancel_handler: Option<Arc<dyn Fn(&str) + Send + Sync>>,
}

impl ServerHandler {
    /// Build a [`ServerHandler`] from an [`MCPServer`] that has handlers registered.
    ///
    /// Returns `None` if the server has no tool handlers.
    pub fn from_server(server: &MCPServer, init_options: InitializationOptions) -> Option<Self> {
        let list_tools = server.list_tools_handler.clone()?;
        let call_tool = server.call_tool_handler.clone()?;
        Some(Self {
            list_tools,
            call_tool,
            list_resources: server.list_resources_handler.clone(),
            read_resource: server.read_resource_handler.clone(),
            init_options,
            cancel_handler: None,
        })
    }

    /// Install a cancellation handler invoked on `notifications/cancelled`.
    ///
    /// The handler receives the stringified `requestId` from the MCP
    /// cancellation notification (falling back to the `id` field if
    /// absent). Typically forwards to
    /// [`AsyncTaskBridge::cancel_session_tasks`].
    pub fn with_cancel_handler(mut self, handler: Arc<dyn Fn(&str) + Send + Sync>) -> Self {
        self.cancel_handler = Some(handler);
        self
    }
}

/// Internal result type to distinguish success from JSON-RPC errors.
enum RpcResult {
    Success(Value),
    Error { code: i32, message: String },
    Notification, // no response needed
}

impl ServerHandler {
    fn rpc_error(code: i32, message: impl Into<String>) -> RpcResult {
        RpcResult::Error {
            code,
            message: message.into(),
        }
    }
}

#[async_trait::async_trait]
impl crate::server::transport::McpHandler for ServerHandler {
    async fn handle_message(&self, message: Value) -> Option<Value> {
        let id = message.get("id").cloned();

        // Extract method — missing or non-string is an invalid request.
        let method =
            match message.get("method").and_then(|v| v.as_str()) {
                Some(m) => m.to_string(),
                None => {
                    // Request (has id) without method → -32600 Invalid Request.
                    // No id + no method → malformed notification, drop silently.
                    return id.map(|id| serde_json::json!({
                    "jsonrpc": "2.0",
                    "id": id,
                    "error": { "code": -32600, "message": "Invalid Request: missing 'method'" }
                }));
                }
            };

        let result = match method.as_str() {
            "initialize" => RpcResult::Success(serde_json::json!({
                "capabilities": {
                    "tools": { "listChanged": true },
                    "resources": { "listChanged": false }
                },
                "serverInfo": {
                    "name": self.init_options.server_name,
                    "version": self.init_options.server_version
                },
                "protocolVersion": "2025-03-26"
            })),
            "tools/list" => {
                let tools = (self.list_tools)();
                let tools_json: Vec<Value> = tools
                    .iter()
                    .map(|t| {
                        let mut obj = serde_json::json!({
                            "name": t.name,
                            "description": t.description,
                            "inputSchema": t.input_schema,
                        });
                        if let Some(ref ann) = t.annotations {
                            obj["annotations"] = serde_json::to_value(ann).unwrap_or_default();
                        }
                        obj
                    })
                    .collect();
                RpcResult::Success(serde_json::json!({ "tools": tools_json }))
            }
            "tools/call" => {
                let params = match message.get("params") {
                    Some(p) => p,
                    None => {
                        return Self::wrap_response(
                            id,
                            ServerHandler::rpc_error(-32602, "Invalid params: missing 'params'"),
                        );
                    }
                };
                let name = match params.get("name").and_then(|v| v.as_str()) {
                    Some(n) => n.to_string(),
                    None => {
                        return Self::wrap_response(
                            id,
                            ServerHandler::rpc_error(
                                -32602,
                                "Invalid params: missing 'name' in params",
                            ),
                        );
                    }
                };
                let arguments = params
                    .get("arguments")
                    .cloned()
                    .unwrap_or(Value::Object(Default::default()));
                let extra = params.get("_meta").cloned();
                let call_result = (self.call_tool)(name, arguments, extra).await;
                let content: Vec<Value> = call_result
                    .content
                    .iter()
                    .map(|c| {
                        serde_json::json!({
                            "type": c.content_type,
                            "text": c.text
                        })
                    })
                    .collect();
                RpcResult::Success(serde_json::json!({
                    "content": content,
                    "isError": call_result.is_error
                }))
            }
            "resources/list" => {
                if let Some(ref handler) = self.list_resources {
                    let resources = handler();
                    let resources_json: Vec<Value> = resources
                        .iter()
                        .map(|r| {
                            serde_json::json!({
                                "uri": r.uri,
                                "name": r.name,
                                "mimeType": r.mime_type
                            })
                        })
                        .collect();
                    RpcResult::Success(serde_json::json!({ "resources": resources_json }))
                } else {
                    RpcResult::Success(serde_json::json!({ "resources": [] }))
                }
            }
            "resources/read" => {
                let params = match message.get("params") {
                    Some(p) => p,
                    None => {
                        return Self::wrap_response(
                            id,
                            ServerHandler::rpc_error(-32602, "Invalid params: missing 'params'"),
                        );
                    }
                };
                let uri = match params.get("uri").and_then(|v| v.as_str()) {
                    Some(u) => u.to_string(),
                    None => {
                        return Self::wrap_response(
                            id,
                            ServerHandler::rpc_error(
                                -32602,
                                "Invalid params: missing 'uri' in params",
                            ),
                        );
                    }
                };
                if let Some(ref handler) = self.read_resource {
                    match handler(uri) {
                        Ok(contents) => {
                            let contents_json: Vec<Value> = contents
                                .iter()
                                .map(|c| {
                                    serde_json::json!({
                                        "content": c.content,
                                        "mimeType": c.mime_type
                                    })
                                })
                                .collect();
                            RpcResult::Success(serde_json::json!({ "contents": contents_json }))
                        }
                        Err(e) => ServerHandler::rpc_error(-32602, e.to_string()),
                    }
                } else {
                    ServerHandler::rpc_error(-32601, "resources not supported")
                }
            }
            "notifications/initialized" => RpcResult::Notification,
            "notifications/cancelled" => {
                // Forward the cancellation to the async-task bridge if a
                // handler is installed. MCP cancellation notifications
                // carry the in-flight `requestId` in `params.requestId`
                // (MCP spec 2025-03-26). We stringify it so the handler
                // can treat it as an opaque session/task key.
                if let Some(h) = &self.cancel_handler {
                    let key = message
                        .get("params")
                        .and_then(|p| p.get("requestId"))
                        .map(|v| match v {
                            Value::String(s) => s.clone(),
                            other => other.to_string(),
                        })
                        .or_else(|| message.get("id").map(|v| v.to_string()))
                        .unwrap_or_default();
                    h(&key);
                }
                RpcResult::Notification
            }
            _ => ServerHandler::rpc_error(-32601, format!("unknown method: {method}")),
        };

        Self::wrap_response(id, result)
    }
}

impl ServerHandler {
    /// Wrap an `RpcResult` into a JSON-RPC 2.0 response envelope.
    fn wrap_response(id: Option<Value>, result: RpcResult) -> Option<Value> {
        match result {
            RpcResult::Notification => None,
            RpcResult::Success(val) => {
                id.map(|id| serde_json::json!({ "jsonrpc": "2.0", "id": id, "result": val }))
            }
            RpcResult::Error { code, message } => id.map(|id| {
                serde_json::json!({
                    "jsonrpc": "2.0",
                    "id": id,
                    "error": { "code": code, "message": message }
                })
            }),
        }
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::server::types::{ServerCapabilities, TextContent, ToolsCapability};

    // ---- TransportKind::from_str tests ----

    #[test]
    fn transport_kind_from_str_stdio() {
        assert_eq!(
            "stdio".parse::<TransportKind>().unwrap(),
            TransportKind::Stdio
        );
    }

    #[test]
    fn transport_kind_from_str_streamable_http() {
        assert_eq!(
            "streamable-http".parse::<TransportKind>().unwrap(),
            TransportKind::StreamableHttp
        );
    }

    #[test]
    fn transport_kind_from_str_sse() {
        assert_eq!("sse".parse::<TransportKind>().unwrap(), TransportKind::Sse);
    }

    #[test]
    fn transport_kind_from_str_case_insensitive() {
        assert_eq!(
            "STDIO".parse::<TransportKind>().unwrap(),
            TransportKind::Stdio
        );
        assert_eq!(
            "Streamable-Http".parse::<TransportKind>().unwrap(),
            TransportKind::StreamableHttp
        );
        assert_eq!("SSE".parse::<TransportKind>().unwrap(), TransportKind::Sse);
    }

    #[test]
    fn transport_kind_from_str_unknown_returns_err() {
        let result = "unknown".parse::<TransportKind>();
        assert!(result.is_err());
        assert_eq!(
            result.unwrap_err().to_string(),
            "unknown transport: \"unknown\""
        );
    }

    // ---- TransportKind::address tests ----

    #[test]
    fn transport_kind_stdio_address() {
        assert_eq!(TransportKind::Stdio.address("127.0.0.1", 8000), "stdio");
    }

    #[test]
    fn transport_kind_streamable_http_address() {
        assert_eq!(
            TransportKind::StreamableHttp.address("127.0.0.1", 8000),
            "http://127.0.0.1:8000"
        );
    }

    #[test]
    fn transport_kind_sse_address() {
        assert_eq!(
            TransportKind::Sse.address("0.0.0.0", 9090),
            "http://0.0.0.0:9090"
        );
    }

    // ---- TransportKind Display tests ----

    #[test]
    fn transport_kind_display() {
        assert_eq!(TransportKind::Stdio.to_string(), "stdio");
        assert_eq!(TransportKind::StreamableHttp.to_string(), "streamable-http");
        assert_eq!(TransportKind::Sse.to_string(), "sse");
    }

    // ---- MCPServerConfig default tests ----

    #[test]
    fn config_default_values() {
        let config = MCPServerConfig::default();
        assert_eq!(config.transport, TransportKind::Stdio);
        assert_eq!(config.host, "127.0.0.1");
        assert_eq!(config.port, 8000);
        assert_eq!(config.name, "apcore-mcp");
        assert_eq!(config.version, None);
        assert!(!config.validate_inputs);
        assert!(config.require_auth);
        assert_eq!(config.tags, None);
        assert_eq!(config.prefix, None);
        assert_eq!(config.exempt_paths, None);
    }

    #[test]
    fn config_can_be_customized() {
        let config = MCPServerConfig {
            transport: TransportKind::StreamableHttp,
            host: "0.0.0.0".to_string(),
            port: 9090,
            name: "my-server".to_string(),
            version: Some("1.0.0".to_string()),
            validate_inputs: true,
            require_auth: false,
            tags: Some(vec!["tag1".to_string()]),
            prefix: Some("my_prefix".to_string()),
            exempt_paths: Some(HashSet::from(["/_health".to_string()])),
            ..Default::default()
        };
        assert_eq!(config.transport, TransportKind::StreamableHttp);
        assert_eq!(config.host, "0.0.0.0");
        assert_eq!(config.port, 9090);
        assert_eq!(config.name, "my-server");
        assert_eq!(config.version, Some("1.0.0".to_string()));
        assert!(config.validate_inputs);
        assert!(!config.require_auth);
        assert_eq!(config.tags.as_ref().unwrap().len(), 1);
        assert_eq!(config.prefix, Some("my_prefix".to_string()));
        assert!(config.exempt_paths.as_ref().unwrap().contains("/_health"));
    }

    // ---- MCPServer with config tests ----

    #[test]
    fn server_new_with_config() {
        let config = MCPServerConfig {
            name: "test-server".to_string(),
            transport: TransportKind::StreamableHttp,
            host: "0.0.0.0".to_string(),
            port: 9090,
            ..Default::default()
        };
        let server = MCPServer::new(config);
        assert_eq!(server.name(), "test-server");
        assert_eq!(server.transport(), TransportKind::StreamableHttp);
        assert_eq!(server.address(), "http://0.0.0.0:9090");
    }

    #[test]
    fn server_with_params_legacy_api() {
        let server = MCPServer::with_params("test", "stdio", "127.0.0.1", 0);
        assert_eq!(server.name(), "test");
        assert_eq!(server.transport(), TransportKind::Stdio);
        assert_eq!(server.address(), "stdio");
    }

    #[test]
    fn server_address_delegates_to_transport_kind() {
        let stdio = MCPServer::new(MCPServerConfig::default());
        assert_eq!(stdio.address(), "stdio");

        let http = MCPServer::new(MCPServerConfig {
            transport: TransportKind::StreamableHttp,
            host: "localhost".to_string(),
            port: 3000,
            ..Default::default()
        });
        assert_eq!(http.address(), "http://localhost:3000");
    }

    #[test]
    fn server_config_accessor() {
        let config = MCPServerConfig {
            validate_inputs: true,
            require_auth: false,
            ..Default::default()
        };
        let server = MCPServer::new(config);
        assert!(server.config().validate_inputs);
        assert!(!server.config().require_auth);
    }

    // ---- Task 4: RegistryOrExecutor and struct completeness ----

    #[test]
    fn registry_or_executor_registry_variant() {
        let reg = Arc::new(apcore::registry::registry::Registry::new());
        let roe = RegistryOrExecutor::Registry(reg);
        assert!(matches!(roe, RegistryOrExecutor::Registry(_)));
    }

    #[test]
    fn registry_or_executor_executor_variant() {
        let exec = Arc::new(apcore::executor::Executor::new(
            apcore::registry::registry::Registry::new(),
            apcore::config::Config::default(),
        ));
        let roe = RegistryOrExecutor::Executor(exec);
        assert!(matches!(roe, RegistryOrExecutor::Executor(_)));
    }

    #[test]
    fn server_with_registry_or_executor_stores_it() {
        let reg = Arc::new(apcore::registry::registry::Registry::new());
        let roe = RegistryOrExecutor::Registry(reg);
        let server = MCPServer::with_registry_or_executor(roe, MCPServerConfig::default());
        assert!(server.registry_or_executor().is_some());
        assert!(matches!(
            server.registry_or_executor().unwrap(),
            RegistryOrExecutor::Registry(_)
        ));
    }

    #[test]
    fn server_new_has_no_registry_or_executor() {
        let server = MCPServer::new(MCPServerConfig::default());
        assert!(server.registry_or_executor().is_none());
    }

    #[test]
    fn server_not_running_after_construction() {
        let server = MCPServer::new(MCPServerConfig::default());
        assert!(!server.is_running());
        assert!(server.join_handle.is_none());
        assert!(server.shutdown_tx.is_none());
    }

    #[test]
    fn server_new_with_stdio_address() {
        let server = MCPServer::new(MCPServerConfig::default());
        assert_eq!(server.address(), "stdio");
    }

    #[test]
    fn server_new_with_streamable_http_address() {
        let config = MCPServerConfig {
            transport: TransportKind::StreamableHttp,
            ..Default::default()
        };
        let server = MCPServer::new(config);
        assert_eq!(server.address(), "http://127.0.0.1:8000");
    }

    #[test]
    fn server_new_with_custom_host_port_address() {
        let config = MCPServerConfig {
            transport: TransportKind::StreamableHttp,
            host: "10.0.0.1".to_string(),
            port: 3000,
            ..Default::default()
        };
        let server = MCPServer::new(config);
        assert_eq!(server.address(), "http://10.0.0.1:3000");
    }

    // ---- Task 5: Server lifecycle tests ----

    #[tokio::test]
    async fn start_is_idempotent() {
        let mut server = MCPServer::new(MCPServerConfig::default());
        server.start().await.unwrap();
        assert!(server.is_running());
        // Second start is a no-op.
        server.start().await.unwrap();
        assert!(server.is_running());
        // Clean up.
        server.stop().await.unwrap();
    }

    #[tokio::test]
    async fn stop_on_unstarted_server_is_noop() {
        let mut server = MCPServer::new(MCPServerConfig::default());
        assert!(!server.is_running());
        // Should not panic or error.
        server.stop().await.unwrap();
        assert!(!server.is_running());
    }

    #[tokio::test]
    async fn start_then_stop_does_not_panic() {
        let mut server = MCPServer::new(MCPServerConfig::default());
        server.start().await.unwrap();
        assert!(server.is_running());
        server.stop().await.unwrap();
        assert!(!server.is_running());
    }

    #[tokio::test]
    async fn after_stop_wait_completes() {
        let mut server = MCPServer::new(MCPServerConfig::default());
        server.start().await.unwrap();
        server.stop().await.unwrap();
        // wait on a stopped server should return immediately.
        server.wait().await.unwrap();
    }

    #[tokio::test]
    async fn wait_on_unstarted_server_returns_immediately() {
        let mut server = MCPServer::new(MCPServerConfig::default());
        server.wait().await.unwrap();
    }

    #[tokio::test]
    async fn stop_then_is_running_false() {
        let mut server = MCPServer::new(MCPServerConfig::default());
        server.start().await.unwrap();
        assert!(server.is_running());
        server.stop().await.unwrap();
        assert!(!server.is_running());
        assert!(server.shutdown_tx.is_none());
    }

    #[tokio::test]
    async fn start_with_handlers_drives_transport_and_stops() {
        // [H-2] When tool handlers are registered, start() drives the
        // configured transport. Use streamable-HTTP on an ephemeral-ish port
        // so the transport future is actually entered, then stop() must race
        // the shutdown channel and tear the server down cleanly.
        // Grab a free port from the OS, then release it so the server can bind.
        let port = {
            let l = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
            l.local_addr().unwrap().port()
        };
        let config = MCPServerConfig {
            transport: TransportKind::StreamableHttp,
            host: "127.0.0.1".to_string(),
            port,
            ..Default::default()
        };
        let mut server = MCPServer::new(config);
        server.list_tools_handler = Some(Arc::new(Vec::new));
        server.call_tool_handler = Some(Arc::new(|_n, _a, _e| {
            Box::pin(async { CallToolResult::new(vec![], false) })
        }));
        assert!(server.has_tool_handlers());

        server.start().await.unwrap();
        assert!(server.is_running());
        // Give the transport a moment to bind.
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        server.stop().await.unwrap();
        assert!(!server.is_running());
    }

    // ---- Handler tests ----

    #[test]
    fn has_tool_handlers_false_by_default() {
        let server = MCPServer::new(MCPServerConfig::default());
        assert!(!server.has_tool_handlers());
    }

    #[test]
    fn has_resource_handlers_false_by_default() {
        let server = MCPServer::new(MCPServerConfig::default());
        assert!(!server.has_resource_handlers());
    }

    #[test]
    fn has_tool_handlers_true_when_both_set() {
        let mut server = MCPServer::new(MCPServerConfig::default());
        server.list_tools_handler = Some(Arc::new(Vec::new));
        // Only list_tools set — should still be false
        assert!(!server.has_tool_handlers());

        server.call_tool_handler = Some(Arc::new(|_name, _args, _extra| {
            Box::pin(async { CallToolResult::new(vec![], false) })
        }));
        assert!(server.has_tool_handlers());
    }

    #[test]
    fn has_resource_handlers_true_when_both_set() {
        let mut server = MCPServer::new(MCPServerConfig::default());
        server.list_resources_handler = Some(Arc::new(Vec::new));
        assert!(!server.has_resource_handlers());

        server.read_resource_handler = Some(Arc::new(|_uri| Ok(vec![])));
        assert!(server.has_resource_handlers());
    }

    #[test]
    fn list_tools_returns_none_when_no_handler() {
        let server = MCPServer::new(MCPServerConfig::default());
        assert!(server.list_tools().is_none());
    }

    #[test]
    fn list_tools_returns_tools_from_handler() {
        let mut server = MCPServer::new(MCPServerConfig::default());
        server.list_tools_handler = Some(Arc::new(|| {
            vec![Tool {
                name: "test-tool".to_string(),
                description: "A test tool".to_string(),
                input_schema: serde_json::json!({"type": "object"}),
                annotations: None,
                meta: None,
            }]
        }));
        let tools = server.list_tools().unwrap();
        assert_eq!(tools.len(), 1);
        assert_eq!(tools[0].name, "test-tool");
    }

    #[tokio::test]
    async fn call_tool_returns_none_when_no_handler() {
        let server = MCPServer::new(MCPServerConfig::default());
        assert!(server.call_tool("foo".into(), Value::Null, None).is_none());
    }

    #[tokio::test]
    async fn call_tool_invokes_handler() {
        let mut server = MCPServer::new(MCPServerConfig::default());
        server.call_tool_handler = Some(Arc::new(|_name, _args, _extra| {
            Box::pin(async move { CallToolResult::new(vec![], false) })
        }));
        let fut = server.call_tool("my-tool".into(), Value::Null, None);
        assert!(fut.is_some());
        let result = fut.unwrap().await;
        assert!(!result.is_error);
    }

    #[test]
    fn list_resources_returns_none_when_no_handler() {
        let server = MCPServer::new(MCPServerConfig::default());
        assert!(server.list_resources().is_none());
    }

    #[test]
    fn read_resource_returns_none_when_no_handler() {
        let server = MCPServer::new(MCPServerConfig::default());
        assert!(server.read_resource("test://uri".into()).is_none());
    }

    // ---- Integration: server start/stop lifecycle with address checks ----

    #[tokio::test]
    async fn address_consistent_before_and_after_start() {
        let config = MCPServerConfig {
            transport: TransportKind::StreamableHttp,
            host: "0.0.0.0".to_string(),
            port: 9090,
            ..Default::default()
        };
        let mut server = MCPServer::new(config);
        let addr_before = server.address();
        server.start().await.unwrap();
        let addr_during = server.address();
        server.stop().await.unwrap();
        let addr_after = server.address();
        assert_eq!(addr_before, "http://0.0.0.0:9090");
        assert_eq!(addr_before, addr_during);
        assert_eq!(addr_during, addr_after);
    }

    #[tokio::test]
    async fn full_lifecycle_start_stop_wait() {
        let mut server = MCPServer::new(MCPServerConfig::default());
        assert!(!server.is_running());

        server.start().await.unwrap();
        assert!(server.is_running());

        server.stop().await.unwrap();
        assert!(!server.is_running());

        // wait after stop is safe
        server.wait().await.unwrap();
        assert!(!server.is_running());
    }

    #[tokio::test]
    async fn double_stop_is_safe() {
        let mut server = MCPServer::new(MCPServerConfig::default());
        server.start().await.unwrap();
        server.stop().await.unwrap();
        // Second stop should not panic or error
        server.stop().await.unwrap();
    }

    #[tokio::test]
    async fn restart_after_stop() {
        let mut server = MCPServer::new(MCPServerConfig::default());
        server.start().await.unwrap();
        assert!(server.is_running());
        server.stop().await.unwrap();
        assert!(!server.is_running());

        // Restart should work
        server.start().await.unwrap();
        assert!(server.is_running());
        server.stop().await.unwrap();
    }

    // ---- TransportKind edge cases ----

    #[test]
    fn transport_kind_from_str_empty_string_is_error() {
        assert!("".parse::<TransportKind>().is_err());
    }

    #[test]
    fn parse_transport_error_display() {
        let err = ParseTransportError("bad".to_string());
        assert_eq!(err.to_string(), "unknown transport: \"bad\"");
    }

    #[test]
    fn transport_kind_clone_and_copy() {
        let t = TransportKind::Sse;
        let t2 = t; // Copy
        let t3 = t;
        assert_eq!(t, t2);
        assert_eq!(t, t3);
    }

    // ---- FactoryError tests ----

    #[test]
    fn factory_error_display() {
        let e1 = FactoryError::ResourceNotFound("foo".into());
        assert_eq!(e1.to_string(), "Resource not found: foo");

        let e2 = FactoryError::UnsupportedScheme("bar".into());
        assert_eq!(e2.to_string(), "Unsupported URI scheme: bar");

        let e3 = FactoryError::Other("something".into());
        assert_eq!(e3.to_string(), "something");
    }

    #[test]
    fn with_params_falls_back_to_stdio_for_unknown_transport() {
        let server = MCPServer::with_params("test", "invalid-transport", "1.2.3.4", 5555);
        assert_eq!(server.transport(), TransportKind::Stdio);
        assert_eq!(server.address(), "stdio");
    }

    // ---- ServerHandler / McpHandler tests ----

    fn make_test_handler() -> ServerHandler {
        use std::pin::Pin;
        let list_tools: Arc<dyn Fn() -> Vec<Tool> + Send + Sync> = Arc::new(|| {
            vec![Tool {
                name: "test.echo".into(),
                description: "Echo test".into(),
                input_schema: serde_json::json!({"type": "object"}),
                annotations: None,
                meta: None,
            }]
        });
        let call_tool: CallToolHandler = Arc::new(|name, args, _extra| {
            Box::pin(async move {
                let text = args
                    .get("text")
                    .and_then(|v| v.as_str())
                    .unwrap_or("no text");
                CallToolResult::new(
                    vec![TextContent::new(format!(
                        "{{\"name\":\"{name}\",\"text\":\"{text}\"}}"
                    ))],
                    false,
                )
            }) as Pin<Box<dyn std::future::Future<Output = CallToolResult> + Send>>
        });
        ServerHandler {
            list_tools,
            call_tool,
            list_resources: None,
            read_resource: None,
            init_options: InitializationOptions {
                server_name: "test-server".into(),
                server_version: "1.0.0".into(),
                capabilities: ServerCapabilities {
                    tools: Some(ToolsCapability { list_changed: true }),
                    resources: None,
                },
            },
            cancel_handler: None,
        }
    }

    #[tokio::test]
    async fn handler_initialize_returns_capabilities() {
        use crate::server::transport::McpHandler;
        let h = make_test_handler();
        let resp = h
            .handle_message(
                serde_json::json!({"jsonrpc":"2.0","id":1,"method":"initialize","params":{}}),
            )
            .await
            .unwrap();
        assert_eq!(resp["id"], 1);
        assert!(resp.get("result").is_some());
        assert!(resp.get("error").is_none());
        assert_eq!(resp["result"]["serverInfo"]["name"], "test-server");
    }

    #[tokio::test]
    async fn handler_tools_list_returns_tools() {
        use crate::server::transport::McpHandler;
        let h = make_test_handler();
        let resp = h
            .handle_message(
                serde_json::json!({"jsonrpc":"2.0","id":2,"method":"tools/list","params":{}}),
            )
            .await
            .unwrap();
        assert_eq!(resp["result"]["tools"][0]["name"], "test.echo");
    }

    #[tokio::test]
    async fn handler_tools_call_success() {
        use crate::server::transport::McpHandler;
        let h = make_test_handler();
        let resp = h
            .handle_message(serde_json::json!({
                "jsonrpc":"2.0","id":3,"method":"tools/call",
                "params":{"name":"test.echo","arguments":{"text":"hello"}}
            }))
            .await
            .unwrap();
        assert_eq!(resp["result"]["isError"], false);
        assert!(resp["result"]["content"][0]["text"]
            .as_str()
            .unwrap()
            .contains("hello"));
    }

    #[tokio::test]
    async fn handler_unknown_method_returns_error() {
        use crate::server::transport::McpHandler;
        let h = make_test_handler();
        let resp = h
            .handle_message(
                serde_json::json!({"jsonrpc":"2.0","id":4,"method":"foo/bar","params":{}}),
            )
            .await
            .unwrap();
        // Must be a top-level "error", NOT nested in "result"
        assert!(
            resp.get("error").is_some(),
            "expected top-level 'error' key"
        );
        assert!(resp.get("result").is_none(), "must not have 'result' key");
        assert_eq!(resp["error"]["code"], -32601);
    }

    #[tokio::test]
    async fn handler_missing_method_returns_invalid_request() {
        use crate::server::transport::McpHandler;
        let h = make_test_handler();
        let resp = h
            .handle_message(serde_json::json!({"jsonrpc":"2.0","id":5}))
            .await
            .unwrap();
        assert!(resp.get("error").is_some());
        assert_eq!(resp["error"]["code"], -32600);
    }

    #[tokio::test]
    async fn handler_tools_call_missing_name_returns_error() {
        use crate::server::transport::McpHandler;
        let h = make_test_handler();
        let resp = h
            .handle_message(serde_json::json!({
                "jsonrpc":"2.0","id":6,"method":"tools/call",
                "params":{"arguments":{}}
            }))
            .await
            .unwrap();
        assert!(resp.get("error").is_some());
        assert_eq!(resp["error"]["code"], -32602);
    }

    #[tokio::test]
    async fn handler_notification_returns_none() {
        use crate::server::transport::McpHandler;
        let h = make_test_handler();
        let resp = h
            .handle_message(
                serde_json::json!({"jsonrpc":"2.0","method":"notifications/initialized"}),
            )
            .await;
        assert!(resp.is_none());
    }

    #[tokio::test]
    async fn cancelled_notification_invokes_cancel_handler() {
        use crate::server::transport::McpHandler;
        use std::sync::atomic::{AtomicUsize, Ordering};
        let counter = Arc::new(AtomicUsize::new(0));
        let last_key = Arc::new(std::sync::Mutex::new(String::new()));
        let c = Arc::clone(&counter);
        let k = Arc::clone(&last_key);
        let h = make_test_handler().with_cancel_handler(Arc::new(move |key: &str| {
            c.fetch_add(1, Ordering::SeqCst);
            let mut guard = k.lock().unwrap();
            *guard = key.to_string();
        }));
        let resp = h
            .handle_message(serde_json::json!({
                "jsonrpc": "2.0",
                "method": "notifications/cancelled",
                "params": {"requestId": "req-42"}
            }))
            .await;
        assert!(resp.is_none());
        assert_eq!(counter.load(Ordering::SeqCst), 1);
        assert_eq!(last_key.lock().unwrap().as_str(), "req-42");
    }
}
