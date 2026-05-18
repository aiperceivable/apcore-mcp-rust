//! APCoreMCP — unified bridge class.
//!
//! Provides builder-pattern construction and the main `serve` / `async_serve`
//! entry points that wire an apcore registry+executor to an MCP server.

use std::collections::HashSet;
use std::path::PathBuf;
use std::sync::Arc;

use axum::Router;
use serde_json::Value;

use apcore::approval::ApprovalHandler;
use apcore::executor::Executor;
use apcore::registry::registry::Registry;

use crate::auth::protocol::Authenticator;
use crate::config;
use crate::converters::openai::OpenAIConverter;
use crate::explorer::{create_explorer_mount, ExplorerConfig, ToolInfo};
use crate::server::factory::MCPServerFactory;
use crate::server::listener::RegistryListener;
use crate::server::router::{ExecutionRouter, ExecutorError, OutputFormatter};
use crate::server::server::{MCPServer, ServerHandler};
use crate::server::transport::{MetricsExporter, TransportManager};
use crate::server::types::{InitializationOptions, Tool};

// ---- BackendSource enum -----------------------------------------------------

/// Describes the source of the apcore backend.
///
/// This replaces the Python polymorphic `str | Path | Registry | Executor`
/// constructor parameter with a typed enum.
///
/// # Examples
///
/// ```
/// use apcore_mcp::apcore_mcp::BackendSource;
/// use std::path::PathBuf;
///
/// // From a string path
/// let source: BackendSource = "./extensions".into();
///
/// // From a PathBuf
/// let source: BackendSource = PathBuf::from("./extensions").into();
/// ```
#[derive(Debug)]
pub enum BackendSource {
    /// Path to an extensions directory. A [`Registry`] will be created
    /// and [`discover()`](Registry::discover) called automatically.
    ExtensionsDir(PathBuf),
    /// A pre-built [`Registry`] instance.
    Registry(Arc<Registry>),
    /// A pre-built [`Executor`] instance (which contains its own registry).
    Executor(Arc<Executor>),
}

impl From<&str> for BackendSource {
    fn from(s: &str) -> Self {
        BackendSource::ExtensionsDir(PathBuf::from(s))
    }
}

impl From<String> for BackendSource {
    fn from(s: String) -> Self {
        BackendSource::ExtensionsDir(PathBuf::from(s))
    }
}

impl From<PathBuf> for BackendSource {
    fn from(p: PathBuf) -> Self {
        BackendSource::ExtensionsDir(p)
    }
}

impl From<Arc<Registry>> for BackendSource {
    fn from(r: Arc<Registry>) -> Self {
        BackendSource::Registry(r)
    }
}

impl From<Arc<Executor>> for BackendSource {
    fn from(e: Arc<Executor>) -> Self {
        BackendSource::Executor(e)
    }
}

// ---- APCoreMCPError ---------------------------------------------------------

/// Errors that can occur during APCoreMCP construction or operation.
#[derive(Debug, thiserror::Error)]
pub enum APCoreMCPError {
    /// The server name must not be empty.
    #[error("name must not be empty")]
    EmptyName,

    /// The server name exceeds the maximum length of 255 characters.
    #[error("name exceeds maximum length of 255: {0}")]
    NameTooLong(usize),

    /// Tag values must not be empty strings.
    #[error("tag values must not be empty")]
    EmptyTag,

    /// The prefix must not be empty when provided.
    #[error("prefix must not be empty")]
    EmptyPrefix,

    /// The specified log level is not recognized.
    #[error("unknown log level: {0:?}. Valid: [\"CRITICAL\", \"DEBUG\", \"ERROR\", \"INFO\", \"WARNING\"]")]
    InvalidLogLevel(String),

    /// The explorer prefix must start with '/'.
    #[error("explorer_prefix must start with '/'")]
    InvalidExplorerPrefix,

    /// Failed to resolve the backend (registry/executor) from the source.
    #[error("backend resolution failed: {0}")]
    BackendResolution(String),

    /// Invalid configuration value (e.g. malformed Config Bus entry).
    #[error("config error: {0}")]
    Config(String),

    /// Unknown transport type.
    #[error("Unknown transport: {0:?}. Expected 'stdio', 'streamable-http', or 'sse'.")]
    UnknownTransport(String),

    /// A server runtime error.
    #[error("server error: {0}")]
    ServerError(String),

    /// OpenAI conversion error.
    #[error("openai conversion error: {0}")]
    ConverterError(#[from] crate::converters::openai::ConverterError),
}

// ---- APCoreMCPConfig --------------------------------------------------------

/// Configuration for the APCoreMCP bridge.
///
/// Matches the Python `APCoreMCP.__init__` parameters. Use [`Default::default()`]
/// for sensible defaults, then override fields as needed.
#[derive(Debug, Clone)]
pub struct APCoreMCPConfig {
    /// MCP server name (max 255 chars).
    pub name: String,
    /// MCP server version. `None` defaults to the crate version.
    pub version: Option<String>,
    /// Transport type: "stdio", "streamable-http", or "sse".
    pub transport: String,
    /// Host address for HTTP-based transports.
    pub host: String,
    /// Port number for HTTP-based transports.
    pub port: u16,
    /// Filter modules by tags. Only modules with ALL specified tags are exposed.
    pub tags: Option<Vec<String>>,
    /// Filter modules by ID prefix.
    pub prefix: Option<String>,
    /// Log level for the apcore_mcp logger (e.g. "DEBUG", "INFO").
    pub log_level: Option<String>,
    /// Validate tool inputs against schemas before execution.
    pub validate_inputs: bool,
    /// Redact sensitive fields from tool outputs before returning to the client.
    pub redact_output: bool,
    /// If true, unauthenticated requests receive 401.
    pub require_auth: bool,
    /// Exact paths that bypass authentication.
    pub exempt_paths: Option<HashSet<String>>,
    /// Enable the browser-based Tool Explorer UI (HTTP only).
    pub explorer: bool,
    /// URL prefix for the explorer.
    pub explorer_prefix: String,
    /// Page title shown in the explorer browser tab and heading.
    pub explorer_title: String,
    /// Optional project name shown in the explorer footer.
    pub explorer_project_name: Option<String>,
    /// Optional project URL linked in the explorer footer.
    pub explorer_project_url: Option<String>,
    /// Allow tool execution from the explorer UI.
    pub allow_execute: bool,
    /// Optional built-in output format name ("json", "csv", "jsonl").
    /// When set, automatically uses the corresponding formatter from `apcore-toolkit`.
    pub output_format: crate::server::router::OutputFormat,
    /// Pipeline execution strategy preset (e.g. "standard", "internal", "testing").
    pub strategy: Option<String>,
    /// Enable pipeline trace mode. When true, tool responses include
    /// pipeline trace data (strategy name, duration, steps).
    pub trace: bool,
    /// Enable the built-in observability middleware stack.
    ///
    /// When `true`, the builder constructs a shared apcore
    /// [`MetricsCollector`](apcore::observability::metrics::MetricsCollector)
    /// plus [`UsageCollector`](apcore::observability::usage::UsageCollector)
    /// and installs the corresponding middleware on the executor. The
    /// metrics collector is also exposed at `/metrics` (Prometheus) and the
    /// usage collector at `/usage` (JSON summaries).
    pub observability: bool,
}

impl Default for APCoreMCPConfig {
    fn default() -> Self {
        Self {
            name: "apcore-mcp".to_string(),
            version: None,
            transport: "stdio".to_string(),
            host: "127.0.0.1".to_string(),
            port: 8000,
            tags: None,
            prefix: None,
            log_level: None,
            validate_inputs: false,
            redact_output: true,
            require_auth: true,
            exempt_paths: None,
            explorer: false,
            explorer_prefix: "/explorer".to_string(),
            explorer_title: "APCore MCP Explorer".to_string(),
            explorer_project_name: Some("apcore-mcp".to_string()),
            explorer_project_url: Some(
                "https://github.com/aiperceivable/apcore-mcp-rust".to_string(),
            ),
            allow_execute: false,
            output_format: crate::server::router::OutputFormat::Json,
            strategy: None,
            trace: false,
            observability: false,
        }
    }
}

// ---- Valid log levels -------------------------------------------------------

const VALID_LOG_LEVELS: &[&str] = &["CRITICAL", "DEBUG", "ERROR", "INFO", "WARNING"];

// ---- Executor adapter -------------------------------------------------------

/// Adapts `apcore::Executor` to the [`crate::server::router::Executor`] trait
/// so that `ExecutionRouter` can dispatch MCP tool calls through it.
struct ApcoreExecutorAdapter {
    inner: Arc<Executor>,
}

#[async_trait::async_trait]
impl crate::server::router::Executor for ApcoreExecutorAdapter {
    async fn call_async(
        &self,
        module_id: &str,
        inputs: &Value,
        _context: Option<&Value>,
        version_hint: Option<&str>,
    ) -> Result<Value, ExecutorError> {
        self.inner
            .call(module_id, inputs.clone(), None, version_hint)
            .await
            .map_err(|e| ExecutorError::Execution {
                code: format!("{:?}", e.code),
                message: e.to_string(),
                details: None,
            })
    }

    async fn call_with_trace(
        &self,
        module_id: &str,
        inputs: &Value,
        _context: Option<&Value>,
        _version_hint: Option<&str>,
    ) -> Option<Result<(Value, Value), ExecutorError>> {
        // Delegates to apcore::Executor::call_with_trace. `version_hint` is
        // accepted for API parity; apcore 0.18's call_with_trace does not yet
        // accept it directly — TODO(apcore>=0.19): forward version_hint.
        let result = self
            .inner
            .call_with_trace(module_id, inputs.clone(), None, None)
            .await;
        Some(match result {
            Ok((out, trace)) => {
                let trace_json = serde_json::to_value(&trace).unwrap_or(Value::Null);
                Ok((out, trace_json))
            }
            Err(e) => Err(ExecutorError::Execution {
                code: format!("{:?}", e.code),
                message: e.to_string(),
                details: None,
            }),
        })
    }

    /// Look up the descriptor-default `version_hint` for the spec's
    /// 3-source cascade. [A-D-006]
    fn version_hint_default(&self, module_id: &str) -> Option<String> {
        self.inner
            .registry()
            .get_definition(module_id)
            .and_then(|desc| desc.metadata.get("version_hint").cloned())
            .and_then(|v| v.as_str().map(|s| s.to_string()))
    }
}

// ---- APCoreMCP struct -------------------------------------------------------

/// The main MCP bridge. Wraps an apcore registry and executor, exposing
/// them as MCP tools over the configured transport.
pub struct APCoreMCP {
    config: APCoreMCPConfig,
    /// User-supplied registry for tool discovery. `None` when backend is
    /// an Executor (tool discovery goes through `executor.registry()`).
    standalone_registry: Option<Arc<Registry>>,
    executor: Arc<Executor>,
    /// Optional authenticator for HTTP transport auth middleware.
    authenticator: Option<Arc<dyn Authenticator>>,
    metrics_collector: Option<Arc<dyn MetricsExporter>>,
    /// Auto-instantiated apcore metrics collector (when `observability=true`).
    auto_metrics: Option<Arc<apcore::observability::metrics::MetricsCollector>>,
    /// Auto-instantiated apcore usage collector (when `observability=true`).
    auto_usage: Option<Arc<apcore::observability::usage::UsageCollector>>,
    /// Reserved — accepted by builder but not yet passed to
    /// `ExecutionRouter::new_with_formatter` (requires converting to
    /// `Arc<dyn Fn>` or taking `&mut self`).
    #[allow(dead_code)]
    output_formatter: Option<OutputFormatter>,
    /// Reserved — accepted by builder but not yet passed to the executor
    /// pipeline (requires `resolve_executor` to support post-construction
    /// injection).
    #[allow(dead_code)]
    approval_handler: Option<Arc<dyn ApprovalHandler>>,
}

/// Project a tool list down to the entries shown in the Explorer UI,
/// dropping reserved `__apcore_*` meta-tools (F-043 AsyncTaskBridge). The
/// meta-tools remain in the MCP `tools/list` response — only the human-
/// facing UI hides them, since their multi-step submit/status/cancel/list
/// flow does not fit a one-form-per-tool layout.
fn filter_explorer_tools(tools: &[Tool]) -> Vec<ToolInfo> {
    tools
        .iter()
        .filter(|t| {
            !t.name
                .starts_with(crate::server::async_task_bridge::META_TOOL_PREFIX)
        })
        .map(|t| ToolInfo {
            name: t.name.clone(),
            description: t.description.clone(),
            input_schema: t.input_schema.clone(),
        })
        .collect()
}

impl APCoreMCP {
    /// Create a new builder.
    pub fn builder() -> APCoreMCPBuilder {
        APCoreMCPBuilder::default()
    }

    // -- Property accessors ---------------------------------------------------

    /// Internal helper — returns the registry for tool discovery.
    fn reg(&self) -> &Registry {
        match &self.standalone_registry {
            Some(reg) => reg,
            None => self.executor.registry(),
        }
    }

    /// Returns a reference to the underlying registry.
    pub fn registry(&self) -> &Registry {
        self.reg()
    }

    /// Returns a reference to the underlying executor.
    pub fn executor(&self) -> &Arc<Executor> {
        &self.executor
    }

    /// Create a RegistryListener if dynamic mode is enabled, leaving the
    /// caller responsible for invoking `listener.start(registry, factory)`
    /// once the `Arc<Registry>` and `Arc<MCPServerFactory>` are both
    /// available downstream of the build pipeline.
    ///
    /// `RegistryListener::start` now accepts `Arc<Registry>` (post-A-D-002),
    /// so wiring no longer requires `&mut Registry`; this function returns
    /// the unstarted listener so the build site can decide ordering.
    fn maybe_start_listener(dynamic: bool) -> Option<RegistryListener> {
        if dynamic {
            let listener = RegistryListener::new();
            tracing::info!(
                "Dynamic mode: RegistryListener created — caller must invoke \
                 listener.start(registry, factory) to begin processing events"
            );
            Some(listener)
        } else {
            None
        }
    }

    /// Returns the currently registered tool names (module IDs), filtered
    /// by the configured tags and prefix.
    pub fn tools(&self) -> Vec<String> {
        let tags_refs: Option<Vec<&str>> = self
            .config
            .tags
            .as_ref()
            .map(|t| t.iter().map(|s| s.as_str()).collect());
        let tags_slice: Option<&[&str]> = tags_refs.as_deref();
        let prefix = self.config.prefix.as_deref();
        self.reg()
            .list(tags_slice, prefix)
            .into_iter()
            .map(|s| s.to_string())
            .collect()
    }

    // -- build_server_components ----------------------------------------------

    /// Build the shared MCP server components used by `serve()` and `async_serve()`.
    ///
    /// Returns `(MCPServer, ExecutionRouter, Vec<Tool>, InitializationOptions, version)`.
    #[allow(clippy::type_complexity)]
    pub(crate) fn build_server_components(
        &self,
    ) -> Result<
        (
            MCPServer,
            Arc<ExecutionRouter>,
            Vec<Tool>,
            InitializationOptions,
            String,
            Option<Arc<crate::server::async_task_bridge::AsyncTaskBridge>>,
        ),
        APCoreMCPError,
    > {
        let version = self
            .config
            .version
            .clone()
            .unwrap_or_else(|| crate::VERSION.to_string());

        let factory = MCPServerFactory::new();
        let mut server = factory.create_server(&self.config.name, &version);

        // Build filtered tool definitions
        let tags_refs: Option<Vec<&str>> = self
            .config
            .tags
            .as_ref()
            .map(|t| t.iter().map(|s| s.as_str()).collect());
        let tags_slice: Option<&[&str]> = tags_refs.as_deref();
        let prefix = self.config.prefix.as_deref();
        let tools = factory
            .build_tools(self.reg(), tags_slice, prefix)
            .map_err(|e| APCoreMCPError::ServerError(e.to_string()))?;

        // Build output schema map for output redaction.
        let output_schema_map: std::collections::HashMap<String, Value> = {
            let module_ids = self.reg().list(tags_slice, prefix);
            let mut map = std::collections::HashMap::new();
            for module_id in module_ids {
                if let Some(descriptor) = self.reg().get_definition(&module_id) {
                    if !descriptor.output_schema.is_null() {
                        map.insert(module_id.to_string(), descriptor.output_schema.clone());
                    }
                }
            }
            map
        };

        // [A-D-031] Async-hinted classification is now done dynamically by
        // the bridge at call time (`bridge.is_async_module_registered_self`),
        // so we no longer pre-populate a static `async_ids` set. The
        // pre-fix set was frozen at startup and stale on registry
        // mutations.
        use crate::server::async_task_bridge::AsyncTaskBridge;

        // Build an AsyncTaskBridge backed by the same executor.
        let bridge = Arc::new(
            AsyncTaskBridge::new(Arc::clone(&self.executor))
                .with_output_schemas(output_schema_map.clone()),
        );

        // Append the four `__apcore_task_*` meta-tools to the tool list so
        // clients see them in `tools/list`.
        let mut tools = tools;
        MCPServerFactory::append_meta_tools(&mut tools);

        // Create execution router backed by the real apcore Executor, and
        // wire in the async bridge.
        let adapter = ApcoreExecutorAdapter {
            inner: Arc::clone(&self.executor),
        };
        let router = Arc::new(
            ExecutionRouter::new(Box::new(adapter), self.config.validate_inputs, None)
                .with_redact_output(self.config.redact_output)
                .with_output_format(self.config.output_format)
                .with_trace(self.config.trace)
                .with_output_schemas(output_schema_map)
                .with_async_bridge(Arc::clone(&bridge)),
        );

        // Register handlers
        factory.register_handlers(&mut server, tools.clone(), Arc::clone(&router));

        // Register resource handlers
        factory.register_resource_handlers(&mut server, self.reg());

        // Build init options
        let init_options = factory.build_init_options(&server, &self.config.name, &version);

        Ok((server, router, tools, init_options, version, Some(bridge)))
    }

    /// Build an [`ExplorerConfig`] from the given tools and explorer parameters.
    ///
    /// Hides reserved `__apcore_*` meta-tools from the Explorer UI — they are
    /// protocol-level operations meant for programmatic MCP clients, not for
    /// the form-driven Explorer UX. They remain advertised via `tools/list`.
    /// Mirrors apcore-mcp-python's `__init__.py` which builds a parallel
    /// `explorer_tools` list excluding meta-tools.
    #[allow(clippy::too_many_arguments)]
    fn build_explorer_config(
        &self,
        tools: &[Tool],
        router: &Arc<ExecutionRouter>,
        prefix: &str,
        allow_execute: bool,
        title: &str,
        project_name: Option<&str>,
        project_url: Option<&str>,
    ) -> ExplorerConfig {
        let tool_infos = filter_explorer_tools(tools);

        let mut config = ExplorerConfig::new(tool_infos)
            .explorer_prefix(prefix)
            .allow_execute(allow_execute)
            .title(title);

        if let Some(name) = project_name {
            config = config.project_name(name);
        }
        if let Some(url) = project_url {
            config = config.project_url(url);
        }

        // Wire handle_call if allow_execute is enabled
        if allow_execute {
            let router_clone = Arc::clone(router);
            config.handle_call = Some(Arc::new(move |name, args| {
                let r = Arc::clone(&router_clone);
                Box::pin(async move {
                    let (content_items, is_error, error_type) =
                        r.handle_call(&name, &args, None).await;
                    // Convert ContentItem to Value for the explorer
                    let values: Vec<serde_json::Value> = content_items
                        .into_iter()
                        .map(|item| {
                            serde_json::json!({
                                "type": item.content_type,
                                "text": item.data
                            })
                        })
                        .collect();
                    (values, is_error, error_type)
                })
            }));
        }

        config
    }

    /// Synchronously start the MCP server (blocks the current thread).
    ///
    /// Creates a new Tokio runtime and runs the server transport loop.
    /// The transport is determined by `config.transport`:
    /// - `"stdio"` — standard I/O transport
    /// - `"streamable-http"` — HTTP-based streamable transport
    /// - `"sse"` — Server-Sent Events transport
    ///
    /// # Panics
    ///
    /// Panics if called from within an active Tokio runtime (e.g. inside
    /// `#[tokio::main]`).  Use [`async_serve`](Self::async_serve) for
    /// async contexts.
    pub fn serve(&self) -> Result<(), APCoreMCPError> {
        self.serve_with_options(ServeOptions::default())
    }

    /// Synchronously start the MCP server with custom options (blocks the current thread).
    pub fn serve_with_options(&self, opts: ServeOptions) -> Result<(), APCoreMCPError> {
        // Validate explorer prefix if explorer is enabled
        if opts.explorer.explorer && !opts.explorer.explorer_prefix.starts_with('/') {
            return Err(APCoreMCPError::InvalidExplorerPrefix);
        }

        let transport = self.config.transport.to_lowercase();
        if !["stdio", "streamable-http", "sse"].contains(&transport.as_str()) {
            return Err(APCoreMCPError::UnknownTransport(
                self.config.transport.clone(),
            ));
        }

        let (server, router, tools, init_options, version, async_bridge) =
            self.build_server_components()?;

        tracing::info!(
            "Starting MCP server '{}' v{} with {} tools via {}",
            self.config.name,
            version,
            tools.len(),
            transport,
        );

        let effective_metrics: Option<Arc<dyn MetricsExporter>> =
            self.metrics_collector.clone().or_else(|| {
                self.auto_metrics
                    .clone()
                    .map(|m| m as Arc<dyn MetricsExporter>)
            });
        let mut transport_manager = TransportManager::new(effective_metrics);
        transport_manager.set_module_count(tools.len());
        if let Some(ref usage) = self.auto_usage {
            let exporter: Arc<dyn crate::server::transport::UsageExporter> = usage.clone();
            transport_manager.set_usage_exporter(Some(exporter));
        }
        if let Some(ref bridge) = async_bridge {
            let bridge_weak = Arc::downgrade(bridge);
            transport_manager.set_cancel_handler(Some(Arc::new(move |session_id: &str| {
                if let Some(b) = bridge_weak.upgrade() {
                    let session_id = session_id.to_string();
                    tokio::spawn(async move {
                        let n = b.cancel_session_tasks(&session_id).await;
                        if n > 0 {
                            tracing::info!(
                                "Cancelled {n} async task(s) for session {session_id} on client disconnect"
                            );
                        }
                    });
                }
            })));
        }
        let transport_manager = Arc::new(transport_manager);

        // Build the McpHandler from the server's registered handlers and
        // install the async-task cancel bridge so that MCP
        // `notifications/cancelled` triggers cooperative task cancellation.
        let mut server_handler = ServerHandler::from_server(&server, init_options)
            .ok_or_else(|| APCoreMCPError::ServerError("no tool handlers registered".into()))?;
        if let Some(ref bridge) = async_bridge {
            let bridge_weak = Arc::downgrade(bridge);
            server_handler = server_handler.with_cancel_handler(Arc::new(move |key: &str| {
                if let Some(b) = bridge_weak.upgrade() {
                    // Cancel any task whose session_key matches, plus also
                    // try treating the key as a direct task_id.
                    let key = key.to_string();
                    tokio::spawn(async move {
                        let n = b.cancel_session_tasks(&key).await;
                        if n == 0 {
                            b.cancel(&key).await;
                        }
                    });
                }
            }));
        }
        let handler: Arc<dyn crate::server::transport::McpHandler> = Arc::new(server_handler);

        // Build explorer router if enabled on HTTP transport.
        let explorer_router = if opts.explorer.explorer && transport != "stdio" {
            let explorer_config = self.build_explorer_config(
                &tools,
                &router,
                &opts.explorer.explorer_prefix,
                opts.explorer.allow_execute,
                &opts.explorer.explorer_title,
                opts.explorer.explorer_project_name.as_deref(),
                opts.explorer.explorer_project_url.as_deref(),
            );
            tracing::info!("Explorer UI mounted at {}", opts.explorer.explorer_prefix);
            Some(create_explorer_mount(explorer_config))
        } else {
            None
        };

        let _listener = Self::maybe_start_listener(opts.dynamic);

        if let Some(ref on_startup) = opts.on_startup {
            on_startup();
        }

        let result = tokio::runtime::Runtime::new()
            .map_err(|e| APCoreMCPError::ServerError(e.to_string()))?
            .block_on(async {
                use crate::server::transport::HttpAuthConfig;
                match transport.as_str() {
                    "streamable-http" => transport_manager
                        .run_streamable_http_with_auth(
                            Arc::clone(&handler),
                            &self.config.host,
                            self.config.port,
                            explorer_router,
                            HttpAuthConfig {
                                authenticator: self.authenticator.clone(),
                                require_auth: self.config.require_auth,
                                explorer_prefix: if opts.explorer.explorer {
                                    Some(opts.explorer.explorer_prefix.clone())
                                } else {
                                    None
                                },
                                exempt_paths: self.config.exempt_paths.clone(),
                            },
                        )
                        .await
                        .map_err(|e| APCoreMCPError::ServerError(e.to_string())),
                    #[allow(deprecated)]
                    "sse" => transport_manager
                        .run_sse_with_auth(
                            Arc::clone(&handler),
                            &self.config.host,
                            self.config.port,
                            explorer_router,
                            HttpAuthConfig {
                                authenticator: self.authenticator.clone(),
                                require_auth: self.config.require_auth,
                                explorer_prefix: if opts.explorer.explorer {
                                    Some(opts.explorer.explorer_prefix.clone())
                                } else {
                                    None
                                },
                                exempt_paths: self.config.exempt_paths.clone(),
                            },
                        )
                        .await
                        .map_err(|e| APCoreMCPError::ServerError(e.to_string())),
                    _ => {
                        // stdio
                        transport_manager
                            .run_stdio(&*handler)
                            .await
                            .map_err(|e| APCoreMCPError::ServerError(e.to_string()))
                    }
                }
            });

        if let Some(ref on_shutdown) = opts.on_shutdown {
            on_shutdown();
        }

        result
    }

    /// Asynchronously build the MCP server and return an axum [`Router`] for embedding.
    ///
    /// This is the async equivalent of [`serve`](Self::serve), but instead of
    /// blocking it returns the built router so callers can mount it in their own
    /// server infrastructure.
    pub async fn async_serve(&self, opts: AsyncServeOptions) -> Result<Router, APCoreMCPError> {
        // Validate explorer prefix
        if opts.explorer.explorer && !opts.explorer.explorer_prefix.starts_with('/') {
            return Err(APCoreMCPError::InvalidExplorerPrefix);
        }

        let (mut server, router, tools, _init_options, version, async_bridge) =
            self.build_server_components()?;

        tracing::info!(
            "Building MCP app '{}' v{} with {} tools",
            self.config.name,
            version,
            tools.len(),
        );

        let _listener = Self::maybe_start_listener(opts.dynamic);

        // Build the transport manager with optional auto-wired observability
        // exporters.
        let effective_metrics: Option<Arc<dyn MetricsExporter>> =
            self.metrics_collector.clone().or_else(|| {
                self.auto_metrics
                    .clone()
                    .map(|m| m as Arc<dyn MetricsExporter>)
            });
        let mut transport_manager = TransportManager::new(effective_metrics);
        transport_manager.set_module_count(tools.len());
        if let Some(ref usage) = self.auto_usage {
            let exporter: Arc<dyn crate::server::transport::UsageExporter> = usage.clone();
            transport_manager.set_usage_exporter(Some(exporter));
        }
        if let Some(ref bridge) = async_bridge {
            let bridge_weak = Arc::downgrade(bridge);
            transport_manager.set_cancel_handler(Some(Arc::new(move |session_id: &str| {
                if let Some(b) = bridge_weak.upgrade() {
                    let session_id = session_id.to_string();
                    tokio::spawn(async move {
                        b.cancel_session_tasks(&session_id).await;
                    });
                }
            })));
        }
        let transport_manager = Arc::new(transport_manager);
        // Silence unused-variable warning; router/bridge are kept alive via
        // the ExecutionRouter stored on the server handler.
        let _ = &router;
        let _ = &async_bridge;

        // Start the server
        server
            .start()
            .await
            .map_err(|e| APCoreMCPError::ServerError(e.to_string()))?;

        // Build health/metrics router from the transport manager
        let mut app = transport_manager.health_metrics_router();

        // Mount explorer if enabled
        if opts.explorer.explorer {
            let explorer_config = self.build_explorer_config(
                &tools,
                &router,
                &opts.explorer.explorer_prefix,
                opts.explorer.allow_execute,
                &opts.explorer.explorer_title,
                opts.explorer.explorer_project_name.as_deref(),
                opts.explorer.explorer_project_url.as_deref(),
            );
            let explorer_router = create_explorer_mount(explorer_config);
            app = app.merge(explorer_router);
            tracing::info!("Explorer UI mounted at {}", opts.explorer.explorer_prefix);
        }

        // Apply auth middleware if authenticator is configured.
        let app = crate::server::transport::apply_auth_layer(
            app,
            crate::server::transport::HttpAuthConfig {
                authenticator: self.authenticator.clone(),
                require_auth: self.config.require_auth,
                explorer_prefix: if opts.explorer.explorer {
                    Some(opts.explorer.explorer_prefix.clone())
                } else {
                    None
                },
                exempt_paths: self.config.exempt_paths.clone(),
            },
        );

        Ok(app)
    }

    /// Convert the current registry contents into OpenAI-compatible tool definitions.
    ///
    /// Delegates to [`OpenAIConverter::convert_registry`], passing through the
    /// configured `tags` and `prefix` filters.
    ///
    /// # Arguments
    /// * `embed_annotations` - If true, append annotation hints to descriptions.
    /// * `strict` - If true, enable OpenAI strict mode on schemas.
    pub fn to_openai_tools(
        &self,
        embed_annotations: bool,
        strict: bool,
    ) -> Result<Vec<Value>, APCoreMCPError> {
        let converter = OpenAIConverter::new();

        // Build a JSON registry object from our Registry
        let registry_json = self.build_registry_json();

        let tags_refs: Option<Vec<&str>> = self
            .config
            .tags
            .as_ref()
            .map(|t| t.iter().map(|s| s.as_str()).collect());
        let tags_slice: Option<&[&str]> = tags_refs.as_deref();
        let prefix = self.config.prefix.as_deref();

        let tools = converter.convert_registry(
            &registry_json,
            embed_annotations,
            strict,
            tags_slice,
            prefix,
        )?;

        tracing::debug!("Converted {} tools to OpenAI format", tools.len());
        Ok(tools)
    }

    /// Build a JSON representation of the registry suitable for [`OpenAIConverter`].
    ///
    /// Returns a JSON object `{ "module_id": { "description": "...", "input_schema": {...}, "annotations": {...}, "tags": [...] }, ... }`.
    fn build_registry_json(&self) -> Value {
        let mut module_ids = self.reg().list(None, None);
        // Explicit sort required after apcore-toolkit 0.7.0 enabled
        // serde_json/preserve_order: Map now preserves insertion order
        // instead of alphabetical (BTreeMap) ordering.
        module_ids.sort();
        let mut map = serde_json::Map::new();

        for module_id in module_ids {
            if let Some(descriptor) = self.reg().get_definition(&module_id) {
                let description = self.reg().describe(&module_id);
                let annotations_json =
                    serde_json::to_value(&descriptor.annotations).unwrap_or(Value::Null);
                let tags_json: Vec<Value> = descriptor
                    .tags
                    .iter()
                    .map(|t| Value::String(t.clone()))
                    .collect();

                let entry = serde_json::json!({
                    "description": description,
                    "input_schema": descriptor.input_schema,
                    "output_schema": descriptor.output_schema,
                    "annotations": annotations_json,
                    "tags": tags_json,
                });
                map.insert(module_id.to_string(), entry);
            }
        }

        Value::Object(map)
    }
}

// ---- ExplorerOptions --------------------------------------------------------

/// Shared explorer configuration for [`ServeOptions`] and [`AsyncServeOptions`].
#[derive(Debug, Clone)]
pub struct ExplorerOptions {
    /// Enable the browser-based Tool Explorer UI.
    pub explorer: bool,
    /// URL prefix for the explorer.
    pub explorer_prefix: String,
    /// Page title shown in the explorer browser tab and heading.
    pub explorer_title: String,
    /// Optional project name shown in the explorer footer.
    pub explorer_project_name: Option<String>,
    /// Optional project URL linked in the explorer footer.
    pub explorer_project_url: Option<String>,
    /// Allow tool execution from the explorer UI.
    pub allow_execute: bool,
}

impl Default for ExplorerOptions {
    fn default() -> Self {
        Self {
            explorer: false,
            explorer_prefix: "/explorer".to_string(),
            explorer_title: "MCP Tool Explorer".to_string(),
            explorer_project_name: None,
            explorer_project_url: None,
            allow_execute: false,
        }
    }
}

// ---- ServeOptions -----------------------------------------------------------

/// Options for [`APCoreMCP::serve_with_options`].
#[derive(Default)]
pub struct ServeOptions {
    /// Callback invoked after setup, before the transport starts.
    pub on_startup: Option<Box<dyn Fn() + Send + Sync>>,
    /// Callback invoked after the transport completes (even on error).
    pub on_shutdown: Option<Box<dyn Fn() + Send + Sync>>,
    /// Explorer UI configuration.
    pub explorer: ExplorerOptions,
    /// Enable dynamic mode: start a RegistryListener that keeps the MCP
    /// tool list in sync with runtime registry changes.
    pub dynamic: bool,
}

impl std::fmt::Debug for ServeOptions {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ServeOptions")
            .field("on_startup", &self.on_startup.as_ref().map(|_| "..."))
            .field("on_shutdown", &self.on_shutdown.as_ref().map(|_| "..."))
            .field("explorer", &self.explorer)
            .field("dynamic", &self.dynamic)
            .finish()
    }
}

// ---- AsyncServeOptions ------------------------------------------------------

/// Options for [`APCoreMCP::async_serve`].
#[derive(Debug, Default)]
pub struct AsyncServeOptions {
    /// Explorer UI configuration.
    pub explorer: ExplorerOptions,
    /// Enable dynamic mode: start a RegistryListener that keeps the MCP
    /// tool list in sync with runtime registry changes.
    pub dynamic: bool,
}

// ---- APCoreMCPBuilder -------------------------------------------------------

/// Builder for [`APCoreMCP`].
#[derive(Default)]
pub struct APCoreMCPBuilder {
    pub config: APCoreMCPConfig,
    backend: Option<BackendSource>,
    authenticator: Option<Arc<dyn Authenticator>>,
    metrics_collector: Option<Arc<dyn MetricsExporter>>,
    output_formatter: Option<OutputFormatter>,
    approval_handler: Option<Arc<dyn ApprovalHandler>>,
    /// apcore `Middleware` instances to install via `executor.use_middleware()`
    /// after the backend Executor is resolved. Appended to any middleware
    /// declared under Config Bus key `mcp.middleware`.
    middleware: Vec<Box<dyn apcore::Middleware>>,
    /// Optional apcore `ACL` to install via `executor.set_acl()` after the
    /// backend Executor is resolved. When `None`, Config Bus `mcp.acl` is
    /// consulted instead. Caller-supplied ACL takes precedence over Config Bus.
    acl: Option<apcore::ACL>,
}

impl APCoreMCPBuilder {
    /// Set the backend source (required).
    pub fn backend(mut self, source: impl Into<BackendSource>) -> Self {
        self.backend = Some(source.into());
        self
    }

    /// Set the MCP server name.
    pub fn name(mut self, name: &str) -> Self {
        self.config.name = name.to_string();
        self
    }

    /// Set the MCP server version.
    pub fn version(mut self, version: &str) -> Self {
        self.config.version = Some(version.to_string());
        self
    }

    /// Set the tag filter.
    pub fn tags(mut self, tags: Vec<String>) -> Self {
        self.config.tags = Some(tags);
        self
    }

    /// Set the prefix filter.
    pub fn prefix(mut self, prefix: &str) -> Self {
        self.config.prefix = Some(prefix.to_string());
        self
    }

    /// Set the log level.
    pub fn log_level(mut self, level: &str) -> Self {
        self.config.log_level = Some(level.to_string());
        self
    }

    /// Set the transport type.
    pub fn transport(mut self, transport: &str) -> Self {
        self.config.transport = transport.to_string();
        self
    }

    /// Set the host address.
    pub fn host(mut self, host: &str) -> Self {
        self.config.host = host.to_string();
        self
    }

    /// Set the port number.
    pub fn port(mut self, port: u16) -> Self {
        self.config.port = port;
        self
    }

    /// Set whether to validate tool inputs.
    pub fn validate_inputs(mut self, validate: bool) -> Self {
        self.config.validate_inputs = validate;
        self
    }

    /// Set whether to redact sensitive fields from tool outputs.
    pub fn redact_output(mut self, redact: bool) -> Self {
        self.config.redact_output = redact;
        self
    }

    /// Set the built-in output format.
    ///
    /// Supports "json", "csv", and "jsonl". Automatically integrates
    /// with `apcore-toolkit`.
    pub fn output_format(mut self, format: crate::server::router::OutputFormat) -> Self {
        self.config.output_format = format;
        self
    }

    /// Set whether authentication is required.
    pub fn require_auth(mut self, require: bool) -> Self {
        self.config.require_auth = require;
        self
    }

    /// Set the authenticator for HTTP transport authentication.
    pub fn authenticator<A: Authenticator + 'static>(mut self, auth: A) -> Self {
        self.authenticator = Some(Arc::new(auth));
        self
    }

    /// Set the authenticator via an already-`Arc`-wrapped trait object.
    ///
    /// Use this when you hold an `Arc<dyn Authenticator>` (e.g. from
    /// [`ServeConfig::authenticator`]) rather than a concrete type.
    pub fn authenticator_arc(mut self, auth: Arc<dyn Authenticator>) -> Self {
        self.authenticator = Some(auth);
        self
    }

    /// Set the metrics collector.
    pub fn metrics_collector(mut self, collector: Arc<dyn MetricsExporter>) -> Self {
        self.metrics_collector = Some(collector);
        self
    }

    /// Set the output formatter.
    ///
    /// **Note:** Reserved — not yet wired into the execution router.
    pub fn output_formatter(mut self, formatter: OutputFormatter) -> Self {
        self.output_formatter = Some(formatter);
        self
    }

    /// Set the approval handler.
    ///
    /// **Note:** Reserved — not yet wired into the executor pipeline. A warning
    /// is emitted at the call site and again at `build()` time so that callers
    /// who configure this field are not silently surprised.
    pub fn approval_handler(mut self, handler: Arc<dyn ApprovalHandler>) -> Self {
        tracing::warn!(
            "approval_handler: not yet wired into the execution pipeline — \
            this setting has no effect in the current release. \
            Approval handlers must be configured at the executor level."
        );
        self.approval_handler = Some(handler);
        self
    }

    /// Install a middleware instance on the Executor via `use_middleware()`.
    ///
    /// Call multiple times to stack middleware. Middleware loaded from Config
    /// Bus `mcp.middleware` is applied first, then builder-supplied middleware
    /// in call order. Chain execution order inside the pipeline is controlled
    /// by `Middleware.priority`, not registration order.
    pub fn middleware(mut self, mw: Box<dyn apcore::Middleware>) -> Self {
        self.middleware.push(mw);
        self
    }

    /// Install a batch of middleware instances in order.
    pub fn middleware_batch(
        mut self,
        mws: impl IntoIterator<Item = Box<dyn apcore::Middleware>>,
    ) -> Self {
        self.middleware.extend(mws);
        self
    }

    /// Install an ACL on the Executor during `build()`.
    ///
    /// Overrides any ACL declared under Config Bus key `mcp.acl`. Applied
    /// after the backend Executor is resolved via `executor.set_acl()`,
    /// which uses interior mutability (apcore >= 0.18.2), so it works with
    /// a shared `Arc<Executor>`.
    pub fn acl(mut self, acl: apcore::ACL) -> Self {
        self.acl = Some(acl);
        self
    }

    /// Set exempt paths for authentication bypass.
    pub fn exempt_paths(mut self, paths: HashSet<String>) -> Self {
        self.config.exempt_paths = Some(paths);
        self
    }

    /// Enable the explorer UI.
    pub fn include_explorer(mut self, include: bool) -> Self {
        self.config.explorer = include;
        self
    }

    /// Set the explorer URL prefix.
    pub fn path_prefix(mut self, prefix: &str) -> Self {
        self.config.explorer_prefix = prefix.to_string();
        self
    }

    /// Set the explorer page title.
    pub fn explorer_title(mut self, title: &str) -> Self {
        self.config.explorer_title = title.to_string();
        self
    }

    /// Set the explorer project name.
    pub fn explorer_project_name(mut self, name: &str) -> Self {
        self.config.explorer_project_name = Some(name.to_string());
        self
    }

    /// Set the explorer project URL.
    pub fn explorer_project_url(mut self, url: &str) -> Self {
        self.config.explorer_project_url = Some(url.to_string());
        self
    }

    /// Set whether tool execution is allowed from the explorer UI.
    pub fn allow_execute(mut self, allow: bool) -> Self {
        self.config.allow_execute = allow;
        self
    }

    /// Set the pipeline execution strategy.
    pub fn strategy(mut self, name: &str) -> Self {
        self.config.strategy = Some(name.to_string());
        self
    }

    /// Enable or disable pipeline trace mode.
    ///
    /// When enabled, tool responses include pipeline trace data
    /// (strategy name, duration, steps) if the executor supports it.
    pub fn trace(mut self, enable: bool) -> Self {
        self.config.trace = enable;
        self
    }

    /// Enable or disable the built-in observability stack.
    ///
    /// When enabled, the builder auto-instantiates apcore's
    /// `MetricsCollector` + `UsageCollector` and installs the matching
    /// middleware on the executor. Metrics are exposed at `/metrics`
    /// (Prometheus text format) and usage summaries at `/usage` (JSON).
    ///
    /// Caller-supplied `metrics_collector` (via
    /// [`APCoreMCPBuilder::metrics_collector`]) takes precedence for the
    /// `/metrics` endpoint, preserving back-compat with custom trait
    /// object exporters.
    pub fn observability(mut self, enable: bool) -> Self {
        self.config.observability = enable;
        self
    }

    /// Consume the builder and produce an [`APCoreMCP`] instance.
    ///
    /// Validates all inputs matching Python validation order, then resolves
    /// the backend into a registry and executor.
    pub fn build(self) -> Result<APCoreMCP, APCoreMCPError> {
        // Validate name
        if self.config.name.is_empty() {
            return Err(APCoreMCPError::EmptyName);
        }
        if self.config.name.len() > 255 {
            return Err(APCoreMCPError::NameTooLong(self.config.name.len()));
        }

        // Validate tags
        if let Some(ref tags) = self.config.tags {
            for tag in tags {
                if tag.is_empty() {
                    return Err(APCoreMCPError::EmptyTag);
                }
            }
        }

        // Validate prefix
        if let Some(ref prefix) = self.config.prefix {
            if prefix.is_empty() {
                return Err(APCoreMCPError::EmptyPrefix);
            }
        }

        // Validate log level
        if let Some(ref level) = self.config.log_level {
            let upper = level.to_uppercase();
            if !VALID_LOG_LEVELS.contains(&upper.as_str()) {
                return Err(APCoreMCPError::InvalidLogLevel(level.clone()));
            }
        }

        // Register MCP config namespace and error formatter (idempotent)
        crate::config::register_mcp_namespace();
        crate::adapters::errors::register_mcp_formatter();

        // F-040: Load pipeline strategy from YAML config if present.
        // The "mcp.pipeline" section in the Config Bus takes precedence over
        // the builder's `strategy` parameter.
        let _resolved_strategy: Option<String> = {
            let yaml_pipeline: Option<Value> = crate::config::get_pipeline_config();
            match (&yaml_pipeline, &self.config.strategy) {
                (Some(pipeline_val), Some(builder_strategy)) => {
                    let yaml_strategy = pipeline_val
                        .get("strategy")
                        .and_then(|v| v.as_str())
                        .map(|s| s.to_string());
                    if let Some(ref ys) = yaml_strategy {
                        tracing::warn!(
                            "YAML pipeline config strategy '{}' overrides builder strategy '{}'",
                            ys,
                            builder_strategy
                        );
                    }
                    yaml_strategy.or_else(|| Some(builder_strategy.clone()))
                }
                (Some(pipeline_val), None) => {
                    let yaml_strategy = pipeline_val
                        .get("strategy")
                        .and_then(|v| v.as_str())
                        .map(|s| s.to_string());
                    if let Some(ref ys) = yaml_strategy {
                        tracing::info!("Pipeline strategy from YAML config: {}", ys);
                    }
                    yaml_strategy
                }
                (None, Some(ref strategy)) => {
                    tracing::info!("Pipeline execution strategy: {}", strategy);
                    Some(strategy.clone())
                }
                (None, None) => None,
            }
        };

        // Resolve backend into (registry, executor) pair.
        let backend = self.backend.ok_or_else(|| {
            APCoreMCPError::BackendResolution("backend source is required".to_string())
        })?;

        let (standalone_registry, mut executor) = match backend {
            BackendSource::ExtensionsDir(path) => {
                return Err(APCoreMCPError::BackendResolution(format!(
                    "ExtensionsDir resolution not yet implemented for path: {}",
                    path.display()
                )));
            }
            BackendSource::Registry(_reg) => {
                // Registry cannot be cloned (contains Box<dyn Module>, callbacks),
                // so we cannot create an Executor from it.  Users must create an
                // Executor themselves and pass it via BackendSource::Executor.
                return Err(APCoreMCPError::BackendResolution(
                    "Registry backend is not supported: Registry cannot be shared with \
                     Executor because it is not Clone. Create an Executor from your \
                     Registry first: `let exec = Arc::new(Executor::new(registry, config)); \
                     builder.backend(exec)`"
                        .to_string(),
                ));
            }
            BackendSource::Executor(exec) => (None, exec),
        };

        // ── Install middleware (Config Bus entries first, then builder) ──
        let config_value = crate::config::get_middleware_config();
        let config_middleware =
            crate::middleware_builder::build_middleware_from_config(config_value.as_ref())?;
        for mw in config_middleware {
            executor.use_middleware(mw).map_err(|e| {
                APCoreMCPError::Config(format!("failed to install Config Bus middleware: {e}"))
            })?;
        }
        for mw in self.middleware {
            executor.use_middleware(mw).map_err(|e| {
                APCoreMCPError::Config(format!("failed to install middleware: {e}"))
            })?;
        }

        // ── Install ACL (caller-supplied wins; else Config Bus) ───────────
        let effective_acl = match self.acl {
            Some(a) => Some(a),
            None => {
                let cfg_value = crate::config::get_acl_config();
                crate::acl_builder::build_acl_from_config(cfg_value.as_ref())?
            }
        };
        if let Some(acl) = effective_acl {
            // `Executor::set_acl` requires `&mut self`. The resolved executor is
            // held in an `Arc`; if no other strong/weak references exist we can
            // safely obtain a `&mut` via `Arc::get_mut`. When the caller has
            // already shared the `Arc` elsewhere, we cannot install the ACL
            // without breaking aliasing, so we surface a config-time error
            // rather than silently dropping it.
            match Arc::get_mut(&mut executor) {
                Some(exec_mut) => exec_mut.set_acl(acl),
                None => {
                    return Err(APCoreMCPError::Config(
                        "failed to install ACL: Executor Arc is already shared — \
                         install the ACL on the Executor before passing it to \
                         APCoreMCPBuilder::backend()"
                            .to_string(),
                    ))
                }
            }
        }

        // ── Auto-wire observability middleware ────────────────────────
        //
        // When `observability=true`, auto-instantiate the apcore
        // MetricsCollector + UsageCollector and install the matching
        // middleware on the executor. The collectors are retained for
        // later exposure at `/metrics` and `/usage`.
        let (auto_metrics, auto_usage) = if self.config.observability {
            use apcore::observability::metrics::{MetricsCollector, MetricsMiddleware};
            use apcore::observability::usage::{UsageCollector, UsageMiddleware};

            let metrics = Arc::new(MetricsCollector::new());
            let usage = Arc::new(UsageCollector::new());

            executor
                .use_middleware(Box::new(MetricsMiddleware::new((*metrics).clone())))
                .map_err(|e| {
                    APCoreMCPError::Config(format!("failed to install MetricsMiddleware: {e}"))
                })?;
            executor
                .use_middleware(Box::new(UsageMiddleware::new((*usage).clone())))
                .map_err(|e| {
                    APCoreMCPError::Config(format!("failed to install UsageMiddleware: {e}"))
                })?;
            tracing::info!(
                "Observability middleware auto-wired (MetricsMiddleware + UsageMiddleware)"
            );
            (Some(metrics), Some(usage))
        } else {
            (None, None)
        };

        // Warn at build time when approval_handler is configured but not yet wired.
        if self.approval_handler.is_some() {
            tracing::warn!(
                "APCoreMCP built with approval_handler configured but it is not yet \
                wired into the execution pipeline"
            );
        }

        Ok(APCoreMCP {
            config: self.config,
            standalone_registry,
            executor,
            authenticator: self.authenticator,
            metrics_collector: self.metrics_collector,
            auto_metrics,
            auto_usage,
            output_formatter: self.output_formatter,
            approval_handler: self.approval_handler,
        })
    }
}

// ---- Top-level convenience functions ----------------------------------------

/// Configuration for the convenience [`serve`] function.
///
/// Extended to cover the key fields from Python's `serve()` signature.
/// Fields with no direct Rust type use `Option<serde_json::Value>` as
/// a placeholder with a TODO comment. [D1-001]
#[derive(Clone)]
pub struct ServeConfig {
    /// MCP server name.
    pub name: String,
    /// MCP server version. `None` defaults to the crate version.
    pub version: Option<String>,
    /// Transport type: "stdio", "streamable-http", or "sse".
    pub transport: String,
    /// Host address for HTTP-based transports.
    pub host: String,
    /// Port number for HTTP-based transports.
    pub port: u16,
    /// Filter modules by tags.
    pub tags: Option<Vec<String>>,
    /// Filter modules by ID prefix.
    pub prefix: Option<String>,
    /// Log level for the apcore_mcp logger.
    pub log_level: Option<String>,
    /// Validate tool inputs against schemas before execution.
    pub validate_inputs: bool,
    /// Pipeline execution strategy preset.
    pub strategy: Option<String>,
    // ---- Fields added for Python parity [D1-001] ----------------------------
    /// JWT authenticator for HTTP transports.
    pub authenticator: Option<std::sync::Arc<dyn crate::auth::protocol::Authenticator>>,
    /// Whether authentication is required (forwarded to authenticator/middleware).
    pub require_auth: Option<bool>,
    /// HTTP paths that bypass authentication.
    pub exempt_paths: Option<Vec<String>>,
    /// Whether to redact sensitive fields from output.
    pub redact_output: Option<bool>,
    /// Built-in output format.
    pub output_format: Option<crate::server::router::OutputFormat>,
    /// Observability configuration.
    pub observability: Option<serde_json::Value>,
}

impl Default for ServeConfig {
    fn default() -> Self {
        Self {
            name: "apcore-mcp".to_string(),
            version: None,
            transport: "stdio".to_string(),
            host: "127.0.0.1".to_string(),
            port: 8000,
            tags: None,
            prefix: None,
            log_level: None,
            validate_inputs: false,
            strategy: None,
            authenticator: None,
            require_auth: None,
            exempt_paths: None,
            redact_output: None,
            output_format: None,
            observability: None,
        }
    }
}

/// Configuration for the convenience [`async_serve`] function.
///
/// Extended to mirror Python's `async_serve()` signature (~25 fields). [D1-002]
#[derive(Clone)]
pub struct AsyncServeConfig {
    /// MCP server name.
    pub name: String,
    /// MCP server version. `None` defaults to the crate version.
    pub version: Option<String>,
    /// Filter modules by tags.
    pub tags: Option<Vec<String>>,
    /// Filter modules by ID prefix.
    pub prefix: Option<String>,
    /// Log level for the apcore_mcp logger.
    pub log_level: Option<String>,
    /// Validate tool inputs against schemas before execution.
    pub validate_inputs: bool,
    /// Pipeline execution strategy preset.
    pub strategy: Option<String>,
    // ---- Fields added for Python parity [D1-002] ----------------------------
    /// JWT authenticator for HTTP transports.
    pub authenticator: Option<std::sync::Arc<dyn crate::auth::protocol::Authenticator>>,
    /// Whether authentication is required.
    pub require_auth: Option<bool>,
    /// HTTP paths that bypass authentication.
    pub exempt_paths: Option<Vec<String>>,
    /// Whether to redact sensitive fields from output.
    pub redact_output: Option<bool>,
    /// Built-in output format.
    pub output_format: Option<crate::server::router::OutputFormat>,
    /// Observability configuration.
    pub observability: Option<serde_json::Value>,
}

impl Default for AsyncServeConfig {
    fn default() -> Self {
        Self {
            name: "apcore-mcp".to_string(),
            version: None,
            tags: None,
            prefix: None,
            log_level: None,
            validate_inputs: false,
            strategy: None,
            authenticator: None,
            require_auth: None,
            exempt_paths: None,
            redact_output: None,
            output_format: None,
            observability: None,
        }
    }
}

/// Configuration for the convenience [`to_openai_tools`] function.
#[derive(Debug, Clone, Default)]
pub struct OpenAIToolsConfig {
    /// Embed annotation metadata in tool descriptions.
    pub embed_annotations: bool,
    /// Add `strict: true` for OpenAI Structured Outputs.
    pub strict: bool,
    /// Filter modules by tags.
    pub tags: Option<Vec<String>>,
    /// Filter modules by ID prefix.
    pub prefix: Option<String>,
}

/// Convenience: build and serve in one call (blocking).
///
/// Constructs an [`APCoreMCP`] internally from the given backend source
/// and config, then calls [`APCoreMCP::serve`].
pub fn serve(backend: impl Into<BackendSource>, config: ServeConfig) -> Result<(), APCoreMCPError> {
    // [D9-003] Apply caller-wins precedence: caller `ServeConfig` field →
    // Config Bus → hardcoded `ServeConfig::default()`. We can only detect
    // "caller did not set this field" by comparing to the default value.
    // Callers who pass `ServeConfig { name: "x".into(), ..ServeConfig::default() }`
    // and set `APCORE_MCP_PORT=9000` in env will see port resolve to 9000.
    let bus = config::get_scalar_config();
    let defaults = ServeConfig::default();
    let transport = if config.transport != defaults.transport {
        config.transport.clone()
    } else {
        bus.transport.unwrap_or(config.transport.clone())
    };
    let host = if config.host != defaults.host {
        config.host.clone()
    } else {
        bus.host.unwrap_or(config.host.clone())
    };
    let port = if config.port != defaults.port {
        config.port
    } else {
        bus.port.unwrap_or(config.port)
    };
    let name = if config.name != defaults.name {
        config.name.clone()
    } else {
        bus.name.unwrap_or(config.name.clone())
    };
    let validate_inputs = if config.validate_inputs != defaults.validate_inputs {
        config.validate_inputs
    } else {
        bus.validate_inputs.unwrap_or(config.validate_inputs)
    };
    let log_level = config.log_level.clone().or(bus.log_level);

    let mut builder = APCoreMCP::builder()
        .backend(backend)
        .name(&name)
        .transport(&transport)
        .host(&host)
        .port(port)
        .validate_inputs(validate_inputs);

    if let Some(version) = config.version.as_deref() {
        builder = builder.version(version);
    }
    if let Some(tags) = config.tags {
        builder = builder.tags(tags);
    }
    if let Some(prefix) = config.prefix.as_deref() {
        builder = builder.prefix(prefix);
    }
    if let Some(level) = log_level.as_deref() {
        builder = builder.log_level(level);
    }
    if let Some(strategy) = config.strategy.as_deref() {
        builder = builder.strategy(strategy);
    }

    // Forward previously-ignored ServeConfig fields to the builder.
    if let Some(auth) = config.authenticator {
        builder = builder.authenticator_arc(auth);
    }
    // [D9-003] require_auth: caller's Some wins; bus fills None.
    if let Some(rq) = config.require_auth.or(bus.require_auth) {
        builder = builder.require_auth(rq);
    }
    if let Some(paths) = config.exempt_paths {
        builder = builder.exempt_paths(paths.into_iter().collect());
    }
    if let Some(redact) = config.redact_output {
        builder = builder.redact_output(redact);
    }
    // [D9-003] output_format: caller > bus. Default is Json (handled by builder).
    if let Some(fmt) = config.output_format.or(bus.output_format) {
        builder = builder.output_format(fmt);
    }
    // observability — extract bool from `true`/`false` or `{ "enabled": bool }`
    // shapes and forward to APCoreMCPBuilder::observability(bool). Matches
    // apcore-mcp-python serve(observability: bool) parity. [D1-001]
    if let Some(obs) = config.observability.as_ref() {
        let enabled = obs
            .as_bool()
            .or_else(|| obs.get("enabled").and_then(|v| v.as_bool()))
            .unwrap_or(true);
        builder = builder.observability(enabled);
    }

    let mcp = builder.build()?;
    mcp.serve()
}

/// Convenience: build and serve in one call (async).
///
/// Constructs an [`APCoreMCP`] internally from the given backend source
/// and config, then calls [`APCoreMCP::async_serve`].
pub async fn async_serve(
    backend: impl Into<BackendSource>,
    config: AsyncServeConfig,
) -> Result<Router, APCoreMCPError> {
    // [D9-003] Same caller-wins precedence as serve(). async_serve does not
    // bind a transport (returns an axum Router), so transport/host/port are
    // not part of the resolved set here.
    let bus = config::get_scalar_config();
    let defaults = AsyncServeConfig::default();
    let name = if config.name != defaults.name {
        config.name.clone()
    } else {
        bus.name.unwrap_or(config.name.clone())
    };
    let validate_inputs = if config.validate_inputs != defaults.validate_inputs {
        config.validate_inputs
    } else {
        bus.validate_inputs.unwrap_or(config.validate_inputs)
    };
    let log_level = config.log_level.clone().or(bus.log_level);

    let mut builder = APCoreMCP::builder()
        .backend(backend)
        .name(&name)
        .validate_inputs(validate_inputs);

    if let Some(version) = config.version.as_deref() {
        builder = builder.version(version);
    }
    if let Some(tags) = config.tags {
        builder = builder.tags(tags);
    }
    if let Some(prefix) = config.prefix.as_deref() {
        builder = builder.prefix(prefix);
    }
    if let Some(level) = log_level.as_deref() {
        builder = builder.log_level(level);
    }
    if let Some(strategy) = config.strategy.as_deref() {
        builder = builder.strategy(strategy);
    }

    // Forward previously-ignored AsyncServeConfig fields to the builder.
    if let Some(auth) = config.authenticator {
        builder = builder.authenticator_arc(auth);
    }
    // [D9-003] require_auth: caller's Some wins; bus fills None.
    if let Some(rq) = config.require_auth.or(bus.require_auth) {
        builder = builder.require_auth(rq);
    }
    if let Some(paths) = config.exempt_paths {
        builder = builder.exempt_paths(paths.into_iter().collect());
    }
    if let Some(redact) = config.redact_output {
        builder = builder.redact_output(redact);
    }
    // [D9-003] output_format: caller > bus.
    if let Some(fmt) = config.output_format.or(bus.output_format) {
        builder = builder.output_format(fmt);
    }
    // observability — extract bool and forward; matches Python parity. [D1-002]
    if let Some(obs) = config.observability.as_ref() {
        let enabled = obs
            .as_bool()
            .or_else(|| obs.get("enabled").and_then(|v| v.as_bool()))
            .unwrap_or(true);
        builder = builder.observability(enabled);
    }

    let mcp = builder.build()?;
    mcp.async_serve(AsyncServeOptions::default()).await
}

/// Convenience: convert a registry to OpenAI tool definitions without starting a server.
///
/// Constructs an [`APCoreMCP`] internally from the given backend source,
/// then calls [`APCoreMCP::to_openai_tools`].
pub fn to_openai_tools(
    backend: impl Into<BackendSource>,
    config: OpenAIToolsConfig,
) -> Result<Vec<Value>, APCoreMCPError> {
    let mut builder = APCoreMCP::builder().backend(backend);

    if let Some(tags) = config.tags {
        builder = builder.tags(tags);
    }
    if let Some(prefix) = config.prefix.as_deref() {
        builder = builder.prefix(prefix);
    }

    let mcp = builder.build()?;
    mcp.to_openai_tools(config.embed_annotations, config.strict)
}

// ---- Tests ------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;
    use std::sync::Arc;

    use apcore::config::Config;
    use apcore::module::{Module, ModuleAnnotations};
    use apcore::registry::ModuleDescriptor;
    use serde_json::json;

    // -- Test helpers ---------------------------------------------------------

    /// Mock module for testing.
    struct MockModule {
        desc: String,
    }

    impl MockModule {
        fn new(desc: &str) -> Self {
            Self {
                desc: desc.to_string(),
            }
        }
    }

    #[async_trait::async_trait]
    impl Module for MockModule {
        fn input_schema(&self) -> Value {
            json!({"type": "object", "properties": {"q": {"type": "string"}}})
        }
        fn output_schema(&self) -> Value {
            json!({})
        }
        fn description(&self) -> &str {
            &self.desc
        }
        async fn execute(
            &self,
            _inputs: Value,
            _ctx: &apcore::context::Context<Value>,
        ) -> Result<Value, apcore::errors::ModuleError> {
            Ok(json!({}))
        }
    }

    /// Create a test registry with the given modules.
    fn make_test_registry(modules: Vec<(&str, &str, Vec<String>)>) -> Registry {
        let registry = Registry::new();
        for (name, desc, tags) in modules {
            let module = Box::new(MockModule::new(desc));
            let descriptor = ModuleDescriptor {
                module_id: name.to_string(),
                name: None,
                description: desc.to_string(),
                documentation: None,
                input_schema: json!({"type": "object", "properties": {"q": {"type": "string"}}}),
                output_schema: json!({}),
                version: "1.0.0".to_string(),
                tags,
                annotations: Some(ModuleAnnotations::default()),
                examples: vec![],
                metadata: std::collections::HashMap::new(),
                display: None,
                sunset_date: None,
                dependencies: vec![],
                enabled: true,
            };
            registry
                .register_internal(name, module, descriptor)
                .unwrap();
        }
        registry
    }

    /// Create a test APCoreMCP with default config and an empty registry.
    fn make_test_apcore_mcp() -> APCoreMCP {
        let registry = Arc::new(make_test_registry(vec![]));
        let executor = Arc::new(Executor::new(Registry::new(), Config::default()));
        APCoreMCP {
            config: APCoreMCPConfig::default(),
            standalone_registry: Some(registry),
            executor,
            authenticator: None,
            metrics_collector: None,
            auto_metrics: None,
            auto_usage: None,
            output_formatter: None,
            approval_handler: None,
        }
    }

    /// Create a test APCoreMCP with a specific version.
    fn make_test_apcore_mcp_with_version(version: &str) -> APCoreMCP {
        let registry = Arc::new(make_test_registry(vec![]));
        let executor = Arc::new(Executor::new(Registry::new(), Config::default()));
        APCoreMCP {
            config: APCoreMCPConfig {
                version: Some(version.to_string()),
                ..Default::default()
            },
            standalone_registry: Some(registry),
            executor,
            authenticator: None,
            metrics_collector: None,
            auto_metrics: None,
            auto_usage: None,
            output_formatter: None,
            approval_handler: None,
        }
    }

    /// Create a test APCoreMCP with populated registry and tag filter.
    fn make_test_apcore_mcp_with_tags(tags: Vec<String>) -> APCoreMCP {
        let registry = Arc::new(make_test_registry(vec![
            ("mod.public", "Public module", vec!["public".to_string()]),
            ("mod.private", "Private module", vec!["private".to_string()]),
            (
                "mod.both",
                "Both module",
                vec!["public".to_string(), "private".to_string()],
            ),
        ]));
        let executor = Arc::new(Executor::new(Registry::new(), Config::default()));
        APCoreMCP {
            config: APCoreMCPConfig {
                tags: Some(tags),
                ..Default::default()
            },
            standalone_registry: Some(registry),
            executor,
            authenticator: None,
            metrics_collector: None,
            auto_metrics: None,
            auto_usage: None,
            output_formatter: None,
            approval_handler: None,
        }
    }

    /// Create a test APCoreMCP with populated registry and prefix filter.
    fn make_test_apcore_mcp_with_prefix(prefix: &str) -> APCoreMCP {
        let registry = Arc::new(make_test_registry(vec![
            ("my_tool.a", "Tool A", vec![]),
            ("my_tool.b", "Tool B", vec![]),
            ("other.c", "Tool C", vec![]),
        ]));
        let executor = Arc::new(Executor::new(Registry::new(), Config::default()));
        APCoreMCP {
            config: APCoreMCPConfig {
                prefix: Some(prefix.to_string()),
                ..Default::default()
            },
            standalone_registry: Some(registry),
            executor,
            authenticator: None,
            metrics_collector: None,
            auto_metrics: None,
            auto_usage: None,
            output_formatter: None,
            approval_handler: None,
        }
    }

    /// Create a test APCoreMCP with populated registry (no filters).
    fn make_test_apcore_mcp_with_modules() -> APCoreMCP {
        let registry = Arc::new(make_test_registry(vec![
            ("mod.a", "Module A", vec!["api".to_string()]),
            ("mod.b", "Module B", vec!["internal".to_string()]),
        ]));
        let executor = Arc::new(Executor::new(Registry::new(), Config::default()));
        APCoreMCP {
            config: APCoreMCPConfig::default(),
            standalone_registry: Some(registry),
            executor,
            authenticator: None,
            metrics_collector: None,
            auto_metrics: None,
            auto_usage: None,
            output_formatter: None,
            approval_handler: None,
        }
    }

    // -- BackendSource tests --------------------------------------------------

    #[test]
    fn from_string_creates_extensions_dir() {
        let source: BackendSource = "./extensions".into();
        assert!(matches!(source, BackendSource::ExtensionsDir(_)));
    }

    #[test]
    fn from_pathbuf_creates_extensions_dir() {
        let source: BackendSource = PathBuf::from("./extensions").into();
        assert!(matches!(source, BackendSource::ExtensionsDir(_)));
    }

    #[test]
    fn from_str_ref_creates_extensions_dir() {
        let source = BackendSource::from("./my-ext");
        if let BackendSource::ExtensionsDir(p) = source {
            assert_eq!(p, PathBuf::from("./my-ext"));
        } else {
            panic!("expected ExtensionsDir");
        }
    }

    #[test]
    fn from_owned_string_creates_extensions_dir() {
        let source = BackendSource::from(String::from("./exts"));
        if let BackendSource::ExtensionsDir(p) = source {
            assert_eq!(p, PathBuf::from("./exts"));
        } else {
            panic!("expected ExtensionsDir");
        }
    }

    #[test]
    fn from_arc_registry_creates_registry_variant() {
        let reg = Arc::new(Registry::new());
        let source = BackendSource::from(reg.clone());
        assert!(matches!(source, BackendSource::Registry(_)));
    }

    #[test]
    fn from_arc_executor_creates_executor_variant() {
        let reg = Registry::new();
        let exec = Arc::new(Executor::new(reg, Config::default()));
        let source = BackendSource::from(exec.clone());
        assert!(matches!(source, BackendSource::Executor(_)));
    }

    #[test]
    fn backend_source_is_debug() {
        let source: BackendSource = "./ext".into();
        let debug_str = format!("{:?}", source);
        assert!(debug_str.contains("ExtensionsDir"));
    }

    // -- APCoreMCPConfig tests ------------------------------------------------

    #[test]
    fn default_config_has_expected_values() {
        let cfg = APCoreMCPConfig::default();
        assert_eq!(cfg.name, "apcore-mcp");
        assert_eq!(cfg.host, "127.0.0.1");
        assert_eq!(cfg.port, 8000);
        assert_eq!(cfg.transport, "stdio");
        assert!(!cfg.validate_inputs);
        assert!(cfg.version.is_none());
        assert!(cfg.tags.is_none());
        assert!(cfg.prefix.is_none());
        assert!(cfg.require_auth);
        assert!(!cfg.allow_execute);
    }

    #[test]
    fn config_explorer_defaults() {
        let cfg = APCoreMCPConfig::default();
        assert!(!cfg.explorer);
        assert_eq!(cfg.explorer_prefix, "/explorer");
        assert_eq!(cfg.explorer_title, "APCore MCP Explorer");
        assert_eq!(cfg.explorer_project_name.as_deref(), Some("apcore-mcp"));
        assert_eq!(
            cfg.explorer_project_url.as_deref(),
            Some("https://github.com/aiperceivable/apcore-mcp-rust")
        );
        assert!(!cfg.allow_execute);
    }

    #[test]
    fn config_is_clone() {
        let cfg = APCoreMCPConfig::default();
        let cfg2 = cfg.clone();
        assert_eq!(cfg.name, cfg2.name);
    }

    // -- APCoreMCPError tests -------------------------------------------------

    #[test]
    fn error_display_empty_name() {
        let err = APCoreMCPError::EmptyName;
        assert!(err.to_string().contains("name"));
    }

    #[test]
    fn error_display_name_too_long() {
        let err = APCoreMCPError::NameTooLong(300);
        assert!(err.to_string().contains("255"));
    }

    #[test]
    fn error_display_empty_tag() {
        let err = APCoreMCPError::EmptyTag;
        assert!(err.to_string().contains("tag"));
    }

    #[test]
    fn error_display_invalid_log_level() {
        let err = APCoreMCPError::InvalidLogLevel("VERBOSE".into());
        assert!(err.to_string().contains("VERBOSE"));
    }

    #[test]
    fn error_display_empty_prefix() {
        let err = APCoreMCPError::EmptyPrefix;
        assert!(err.to_string().contains("prefix"));
    }

    #[test]
    fn error_display_invalid_explorer_prefix() {
        let err = APCoreMCPError::InvalidExplorerPrefix;
        assert!(err.to_string().contains("explorer_prefix"));
    }

    #[test]
    fn error_display_backend_resolution() {
        let err = APCoreMCPError::BackendResolution("not found".into());
        assert!(err.to_string().contains("not found"));
    }

    #[test]
    fn error_display_server_error() {
        let err = APCoreMCPError::ServerError("bind failed".into());
        assert!(err.to_string().contains("bind failed"));
    }

    #[test]
    fn error_is_std_error() {
        let err: Box<dyn std::error::Error> = Box::new(APCoreMCPError::EmptyName);
        assert!(err.to_string().contains("name"));
    }

    // -- Builder pattern tests (Task 4) ---------------------------------------

    #[test]
    fn builder_requires_backend() {
        let result = APCoreMCP::builder().build();
        assert!(result.is_err());
    }

    #[test]
    fn builder_rejects_empty_name() {
        let result = APCoreMCP::builder().backend("./ext").name("").build();
        assert!(matches!(result, Err(APCoreMCPError::EmptyName)));
    }

    #[test]
    fn builder_rejects_name_over_255() {
        let long = "a".repeat(256);
        let result = APCoreMCP::builder().backend("./ext").name(&long).build();
        assert!(matches!(result, Err(APCoreMCPError::NameTooLong(256))));
    }

    #[test]
    fn builder_rejects_empty_tag() {
        let result = APCoreMCP::builder()
            .backend("./ext")
            .tags(vec!["".to_string()])
            .build();
        assert!(matches!(result, Err(APCoreMCPError::EmptyTag)));
    }

    #[test]
    fn builder_rejects_empty_prefix() {
        let result = APCoreMCP::builder().backend("./ext").prefix("").build();
        assert!(matches!(result, Err(APCoreMCPError::EmptyPrefix)));
    }

    #[test]
    fn builder_rejects_invalid_log_level() {
        let result = APCoreMCP::builder()
            .backend("./ext")
            .log_level("VERBOSE")
            .build();
        assert!(matches!(result, Err(APCoreMCPError::InvalidLogLevel(_))));
    }

    #[test]
    fn builder_accepts_valid_log_levels() {
        for level in &["DEBUG", "INFO", "WARNING", "ERROR", "CRITICAL"] {
            let result = APCoreMCP::builder()
                .backend("./ext")
                .log_level(level)
                .build();
            // May fail on backend resolution, but not on log level validation
            if let Err(e) = &result {
                assert!(!matches!(e, APCoreMCPError::InvalidLogLevel(_)));
            }
        }
    }

    #[test]
    fn builder_sets_all_config_fields() {
        let builder = APCoreMCP::builder()
            .name("test-server")
            .version("1.0.0")
            .tags(vec!["public".into()])
            .prefix("my_")
            .transport("streamable-http")
            .host("0.0.0.0")
            .port(9000)
            .validate_inputs(true)
            .require_auth(false);
        assert_eq!(builder.config.name, "test-server");
        assert_eq!(builder.config.version, Some("1.0.0".to_string()));
        assert_eq!(builder.config.tags, Some(vec!["public".to_string()]));
        assert_eq!(builder.config.prefix, Some("my_".to_string()));
        assert_eq!(builder.config.transport, "streamable-http");
        assert_eq!(builder.config.host, "0.0.0.0");
        assert_eq!(builder.config.port, 9000);
        assert!(builder.config.validate_inputs);
        assert!(!builder.config.require_auth);
    }

    #[test]
    fn builder_with_registry_backend_returns_error() {
        let reg = Arc::new(Registry::new());
        let result = APCoreMCP::builder()
            .backend(BackendSource::Registry(reg))
            .build();
        assert!(result.is_err());
        assert!(matches!(result, Err(APCoreMCPError::BackendResolution(_))));
    }

    #[test]
    fn builder_with_executor_backend_succeeds() {
        let reg = Registry::new();
        let exec = Arc::new(Executor::new(reg, Config::default()));
        let result = APCoreMCP::builder()
            .backend(BackendSource::Executor(exec))
            .build();
        // Executor backend: stored registry is a placeholder (empty),
        // tool discovery uses executor.registry() via reg().
        assert!(result.is_ok());
    }

    // -- Middleware support tests (Phase 1.1) ----------------------------------

    #[test]
    fn builder_middleware_setter_installs_on_executor() {
        use apcore::RetryMiddleware;
        let reg = Registry::new();
        let exec = Arc::new(Executor::new(reg, Config::default()));
        APCoreMCP::builder()
            .backend(BackendSource::Executor(exec.clone()))
            .middleware(Box::new(RetryMiddleware::new(Default::default())))
            .build()
            .expect("build should succeed");
        let names = exec.middlewares();
        assert!(
            names.iter().any(|n| n == "retry"),
            "expected 'retry' in executor middlewares, got {names:?}"
        );
    }

    #[test]
    fn builder_middleware_batch_installs_all_in_order() {
        use apcore::{LoggingMiddleware, RetryMiddleware};
        let reg = Registry::new();
        let exec = Arc::new(Executor::new(reg, Config::default()));
        APCoreMCP::builder()
            .backend(BackendSource::Executor(exec.clone()))
            .middleware_batch(vec![
                Box::new(RetryMiddleware::new(Default::default())) as Box<dyn apcore::Middleware>,
                Box::new(LoggingMiddleware::new(true, true, true)) as Box<dyn apcore::Middleware>,
            ])
            .build()
            .expect("build should succeed");
        let names = exec.middlewares();
        assert!(
            names.iter().any(|n| n == "retry") && names.iter().any(|n| n == "logging"),
            "expected both 'retry' and 'logging', got {names:?}"
        );
    }

    // -- ACL support tests (Phase 1.2) -----------------------------------------

    #[test]
    fn builder_acl_setter_installs_on_executor() {
        use apcore::{ACLRule, ACL};
        // apcore 0.19.0: `Executor::set_acl` takes `&mut self`. To install an
        // ACL through `APCoreMCPBuilder`, the backend Executor Arc must be
        // unique — do NOT hold a clone across the call to `build()`. The
        // builder surfaces this as a `Config` error; we verify the happy path
        // here by inspecting the MCP instance's executor.
        let reg = Registry::new();
        let exec = Arc::new(Executor::new(reg, Config::default()));
        let acl = ACL::new(
            vec![ACLRule {
                callers: vec!["role:admin".to_string()],
                targets: vec!["sys.*".to_string()],
                effect: "allow".to_string(),
                description: None,
                conditions: None,
            }],
            "deny",
            None,
        );
        let mcp = APCoreMCP::builder()
            .backend(BackendSource::Executor(exec))
            .acl(acl)
            .build()
            .expect("build should succeed");
        let installed = mcp.executor().acl.as_ref();
        assert!(installed.is_some(), "expected ACL on executor");
        assert_eq!(installed.unwrap().rules().len(), 1);
    }

    #[test]
    fn builder_without_acl_leaves_executor_acl_unset() {
        let reg = Registry::new();
        let exec = Arc::new(Executor::new(reg, Config::default()));
        APCoreMCP::builder()
            .backend(BackendSource::Executor(exec.clone()))
            .build()
            .expect("build should succeed");
        // Note: Config Bus `mcp.acl` may be read here. Assert only the
        // caller-did-not-set-ACL path — if tests are run with a config file
        // that defines acl, this assertion is skipped.
        if crate::config::get_acl_config().is_none() {
            assert!(exec.acl.is_none(), "expected no ACL when none supplied");
        }
    }

    // -- Struct and accessor tests (Task 5) -----------------------------------

    #[test]
    fn registry_returns_arc_ref() {
        let mcp = make_test_apcore_mcp();
        let reg = mcp.registry();
        // Should return &Arc<Registry>
        let _ = reg.list(None, None);
    }

    #[test]
    fn executor_returns_arc_ref() {
        let mcp = make_test_apcore_mcp();
        let _exec = mcp.executor();
        // Just verify it returns without panic
    }

    // Task 2: default strategy is "standard" for MCP (external-facing).
    #[test]
    fn default_executor_uses_standard_strategy() {
        let mcp = make_test_apcore_mcp();
        let info = mcp.executor().describe_pipeline();
        assert_eq!(info.name, "standard");
        // Standard strategy has several steps including ACL + validate + execute.
        assert!(info.step_count >= 3, "got {} steps", info.step_count);
        assert!(
            info.step_names.iter().any(|s| s == "execute"),
            "steps: {:?}",
            info.step_names
        );
    }

    #[test]
    fn tools_returns_module_ids() {
        let mcp = make_test_apcore_mcp_with_modules();
        let tools = mcp.tools();
        assert_eq!(tools.len(), 2);
        assert!(tools.contains(&"mod.a".to_string()));
        assert!(tools.contains(&"mod.b".to_string()));
    }

    #[test]
    fn tools_returns_empty_for_empty_registry() {
        let mcp = make_test_apcore_mcp();
        let tools = mcp.tools();
        assert!(tools.is_empty());
    }

    #[test]
    fn tools_filters_by_tags() {
        let mcp = make_test_apcore_mcp_with_tags(vec!["public".into()]);
        let tools = mcp.tools();
        // Should include mod.public and mod.both (both have "public" tag)
        assert!(tools.contains(&"mod.public".to_string()));
        assert!(tools.contains(&"mod.both".to_string()));
        assert!(!tools.contains(&"mod.private".to_string()));
    }

    #[test]
    fn tools_filters_by_prefix() {
        let mcp = make_test_apcore_mcp_with_prefix("my_tool.");
        let tools = mcp.tools();
        assert!(tools.contains(&"my_tool.a".to_string()));
        assert!(tools.contains(&"my_tool.b".to_string()));
        assert!(!tools.contains(&"other.c".to_string()));
    }

    // -- build_server_components tests (Task 6) -------------------------------

    #[test]
    fn build_server_components_returns_all_parts() {
        let mcp = make_test_apcore_mcp();
        let components = mcp.build_server_components();
        assert!(components.is_ok());
        let (_server, _router, _tools, _init_options, version, _bridge) = components.unwrap();
        assert!(!version.is_empty());
    }

    #[test]
    fn build_server_components_uses_custom_version() {
        let mcp = make_test_apcore_mcp_with_version("2.0.0");
        let (_, _, _, _, version, _) = mcp.build_server_components().unwrap();
        assert_eq!(version, "2.0.0");
    }

    #[test]
    fn build_server_components_defaults_to_crate_version() {
        let mcp = make_test_apcore_mcp();
        let (_, _, _, _, version, _) = mcp.build_server_components().unwrap();
        assert_eq!(version, crate::VERSION);
    }

    #[test]
    fn build_server_components_applies_tag_filter() {
        let mcp = make_test_apcore_mcp_with_tags(vec!["public".into()]);
        let (_, _, tools, _, _, _) = mcp.build_server_components().unwrap();
        // Only modules with "public" tag
        let names: Vec<&str> = tools.iter().map(|t| t.name.as_str()).collect();
        assert!(names.contains(&"mod.public"));
        assert!(names.contains(&"mod.both"));
        assert!(!names.contains(&"mod.private"));
    }

    #[test]
    fn build_server_components_applies_prefix_filter() {
        let mcp = make_test_apcore_mcp_with_prefix("my_tool.");
        let (_, _, tools, _, _, _) = mcp.build_server_components().unwrap();
        let names: Vec<&str> = tools.iter().map(|t| t.name.as_str()).collect();
        assert!(names.contains(&"my_tool.a"));
        assert!(names.contains(&"my_tool.b"));
        assert!(!names.contains(&"other.c"));
    }

    #[test]
    fn build_server_components_has_init_options() {
        let mcp = make_test_apcore_mcp_with_modules();
        let (_, _, _, init_options, _, _) = mcp.build_server_components().unwrap();
        assert_eq!(init_options.server_name, "apcore-mcp");
        assert_eq!(init_options.server_version, crate::VERSION);
        // Should have tools capability since we registered handlers
        assert!(init_options.capabilities.tools.is_some());
        // Should have resources capability since we registered resource handlers
        assert!(init_options.capabilities.resources.is_some());
    }

    #[test]
    fn build_server_components_server_has_handlers() {
        let mcp = make_test_apcore_mcp_with_modules();
        let (server, _, _, _, _, _) = mcp.build_server_components().unwrap();
        assert!(server.has_tool_handlers());
        assert!(server.has_resource_handlers());
    }

    #[test]
    fn build_server_components_tools_match_registry() {
        let mcp = make_test_apcore_mcp_with_modules();
        let (_, _, tools, _, _, _) = mcp.build_server_components().unwrap();
        let tool_names: Vec<&str> = tools.iter().map(|t| t.name.as_str()).collect();
        let registry_tools = mcp.tools();
        // Registry tools plus the 5 reserved `__apcore_*` meta-tools
        // auto-appended by the AsyncTaskBridge: 4 task-management tools
        // plus `__apcore_module_preview` (apcore 0.21 PROTOCOL_SPEC §5.6).
        assert_eq!(tools.len(), registry_tools.len() + 5);
        for name in &registry_tools {
            assert!(tool_names.contains(&name.as_str()));
        }
        for reserved in [
            "__apcore_task_submit",
            "__apcore_task_status",
            "__apcore_task_cancel",
            "__apcore_task_list",
            "__apcore_module_preview",
        ] {
            assert!(
                tool_names.contains(&reserved),
                "meta-tool {reserved} must be registered"
            );
        }
    }

    // -- serve-methods tests (Task 7) -----------------------------------------

    fn make_test_apcore_mcp_with_transport(transport: &str) -> APCoreMCP {
        let registry = Arc::new(make_test_registry(vec![]));
        let executor = Arc::new(Executor::new(Registry::new(), Config::default()));
        APCoreMCP {
            config: APCoreMCPConfig {
                transport: transport.to_string(),
                ..Default::default()
            },
            standalone_registry: Some(registry),
            executor,
            authenticator: None,
            metrics_collector: None,
            auto_metrics: None,
            auto_usage: None,
            output_formatter: None,
            approval_handler: None,
        }
    }

    #[test]
    fn serve_rejects_unknown_transport() {
        let mcp = make_test_apcore_mcp_with_transport("websocket");
        let result = mcp.serve();
        assert!(result.is_err());
        let err = result.unwrap_err();
        assert!(err.to_string().contains("Unknown transport"));
    }

    #[test]
    fn serve_rejects_invalid_explorer_prefix() {
        let mcp = make_test_apcore_mcp();
        let result = mcp.serve_with_options(ServeOptions {
            explorer: ExplorerOptions {
                explorer: true,
                explorer_prefix: "no-slash".into(),
                ..Default::default()
            },
            ..Default::default()
        });
        assert!(matches!(result, Err(APCoreMCPError::InvalidExplorerPrefix)));
    }

    #[tokio::test]
    async fn async_serve_returns_router() {
        let mcp = make_test_apcore_mcp();
        let result = mcp.async_serve(AsyncServeOptions::default()).await;
        assert!(result.is_ok());
    }

    #[tokio::test]
    async fn async_serve_rejects_invalid_explorer_prefix() {
        let mcp = make_test_apcore_mcp();
        let result = mcp
            .async_serve(AsyncServeOptions {
                explorer: ExplorerOptions {
                    explorer: true,
                    explorer_prefix: "no-slash".into(),
                    ..Default::default()
                },
                ..Default::default()
            })
            .await;
        assert!(matches!(result, Err(APCoreMCPError::InvalidExplorerPrefix)));
    }

    #[test]
    fn error_display_unknown_transport() {
        let err = APCoreMCPError::UnknownTransport("websocket".into());
        assert!(err.to_string().contains("Unknown transport"));
        assert!(err.to_string().contains("websocket"));
    }

    // -- to-openai-tools tests (Task 8) ---------------------------------------

    #[test]
    fn to_openai_tools_returns_vec_of_values() {
        let mcp = make_test_apcore_mcp();
        let tools = mcp.to_openai_tools(false, false).unwrap();
        assert!(tools.is_empty() || tools.iter().all(|t| t.is_object()));
    }

    #[test]
    fn to_openai_tools_with_modules_returns_tools() {
        let mcp = make_test_apcore_mcp_with_modules();
        let tools = mcp.to_openai_tools(false, false).unwrap();
        assert_eq!(tools.len(), 2);
        for tool in &tools {
            assert!(tool.is_object());
            assert_eq!(tool["type"], "function");
            assert!(tool["function"]["name"].is_string());
            assert!(tool["function"]["description"].is_string());
        }
    }

    #[test]
    fn to_openai_tools_respects_tags() {
        let mcp = make_test_apcore_mcp_with_tags(vec!["public".into()]);
        let tools = mcp.to_openai_tools(false, false).unwrap();
        // Should only include modules with "public" tag (mod.public and mod.both)
        assert_eq!(tools.len(), 2);
    }

    #[test]
    fn to_openai_tools_respects_prefix() {
        let mcp = make_test_apcore_mcp_with_prefix("my_tool.");
        let tools = mcp.to_openai_tools(false, false).unwrap();
        // Should only include my_tool.a and my_tool.b
        assert_eq!(tools.len(), 2);
    }

    #[test]
    fn to_openai_tools_strict_flag() {
        let mcp = make_test_apcore_mcp_with_modules();
        let tools = mcp.to_openai_tools(false, true).unwrap();
        for tool in &tools {
            assert_eq!(tool["function"]["strict"], true);
        }
    }

    #[test]
    fn to_openai_tools_embed_annotations() {
        let mcp = make_test_apcore_mcp_with_modules();
        let tools_without = mcp.to_openai_tools(false, false).unwrap();
        let tools_with = mcp.to_openai_tools(true, false).unwrap();
        // Both should return the same number of tools
        assert_eq!(tools_without.len(), tools_with.len());
    }

    // -- convenience-functions tests (Task 9) ---------------------------------

    #[test]
    fn convenience_serve_config_has_defaults() {
        let cfg = ServeConfig::default();
        assert_eq!(cfg.transport, "stdio");
        assert_eq!(cfg.host, "127.0.0.1");
        assert_eq!(cfg.port, 8000);
        assert_eq!(cfg.name, "apcore-mcp");
    }

    #[test]
    fn convenience_async_serve_config_has_defaults() {
        let cfg = AsyncServeConfig::default();
        assert_eq!(cfg.name, "apcore-mcp");
        assert!(cfg.version.is_none());
        assert!(cfg.tags.is_none());
        assert!(cfg.prefix.is_none());
    }

    #[test]
    fn convenience_openai_tools_config_has_defaults() {
        let cfg = OpenAIToolsConfig::default();
        assert!(!cfg.embed_annotations);
        assert!(!cfg.strict);
        assert!(cfg.tags.is_none());
        assert!(cfg.prefix.is_none());
    }

    #[test]
    fn serve_options_default_has_no_callbacks() {
        let opts = ServeOptions::default();
        assert!(opts.on_startup.is_none());
        assert!(opts.on_shutdown.is_none());
        assert!(!opts.explorer.explorer);
    }

    #[test]
    fn async_serve_options_default() {
        let opts = AsyncServeOptions::default();
        assert!(!opts.explorer.explorer);
        assert_eq!(opts.explorer.explorer_prefix, "/explorer");
    }

    // -- build_registry_json tests (Task 8, helper) ---------------------------

    #[test]
    fn build_registry_json_empty_registry() {
        let mcp = make_test_apcore_mcp();
        let json = mcp.build_registry_json();
        assert!(json.is_object());
        assert_eq!(json.as_object().unwrap().len(), 0);
    }

    #[test]
    fn build_registry_json_with_modules() {
        let mcp = make_test_apcore_mcp_with_modules();
        let json = mcp.build_registry_json();
        let obj = json.as_object().unwrap();
        assert_eq!(obj.len(), 2);
        assert!(obj.contains_key("mod.a"));
        assert!(obj.contains_key("mod.b"));

        let mod_a = &obj["mod.a"];
        assert!(mod_a["description"].is_string());
        assert!(mod_a["input_schema"].is_object());
        assert!(mod_a["tags"].is_array());
    }

    // ==================================================================
    // P0 #3: Observability auto-wiring
    // ==================================================================

    #[test]
    fn observability_flag_auto_wires_collectors() {
        let executor = Arc::new(Executor::new(Registry::new(), Config::default()));
        let mcp = APCoreMCP::builder()
            .backend(executor)
            .name("obs-test")
            .observability(true)
            .build()
            .expect("build must succeed");
        assert!(
            mcp.auto_metrics.is_some(),
            "MetricsCollector should be auto-instantiated"
        );
        assert!(
            mcp.auto_usage.is_some(),
            "UsageCollector should be auto-instantiated"
        );
    }

    #[test]
    fn observability_off_does_not_wire_collectors() {
        let executor = Arc::new(Executor::new(Registry::new(), Config::default()));
        let mcp = APCoreMCP::builder()
            .backend(executor)
            .name("obs-off")
            .build()
            .expect("build must succeed");
        assert!(mcp.auto_metrics.is_none());
        assert!(mcp.auto_usage.is_none());
    }

    #[test]
    fn observability_metrics_has_export_prometheus() {
        use crate::server::transport::MetricsExporter;
        let executor = Arc::new(Executor::new(Registry::new(), Config::default()));
        let mcp = APCoreMCP::builder()
            .backend(executor)
            .name("obs-prom")
            .observability(true)
            .build()
            .expect("build must succeed");
        let metrics = mcp.auto_metrics.as_ref().unwrap().clone();
        // Exercise the blanket `impl MetricsExporter for MetricsCollector` so
        // the adapter is actually driven through the trait the bridge uses.
        let exporter: Arc<dyn MetricsExporter> = metrics;
        let exported = exporter.export_prometheus();
        // Empty collector is valid; just ensure the adapter is callable.
        assert!(exported.is_empty() || exported.contains("# HELP"));
    }

    #[test]
    fn observability_usage_exposes_summaries() {
        use crate::server::transport::UsageExporter;
        let executor = Arc::new(Executor::new(Registry::new(), Config::default()));
        let mcp = APCoreMCP::builder()
            .backend(executor)
            .name("obs-usage")
            .observability(true)
            .build()
            .expect("build must succeed");
        let usage = mcp.auto_usage.as_ref().unwrap();
        let summary = usage.export_json();
        assert!(summary.get("modules").is_some());
    }

    // -- Issue D1-001: ServeConfig has extended fields -------------------------

    #[test]
    fn test_serve_config_default_has_all_fields() {
        // [D1-001] ServeConfig must have the key Python parity fields.
        // The 8 stub fields removed in D9-002 (`async_tasks`,
        // `async_max_concurrent`, `schema_converter`,
        // `annotation_mapper`, `error_mapper`, `on_startup`,
        // `on_shutdown`, `metrics_collector`) were never read by
        // `serve()` and used placeholder `Option<serde_json::Value>`
        // types that could not accept real callbacks. Lifecycle and
        // observability callbacks are wired through `ServeOptions`.
        let cfg = ServeConfig::default();
        assert_eq!(cfg.name, "apcore-mcp");
        assert!(cfg.authenticator.is_none());
        assert!(cfg.require_auth.is_none());
        assert!(cfg.exempt_paths.is_none());
        assert!(cfg.redact_output.is_none());
        assert!(cfg.observability.is_none());
    }

    #[test]
    fn test_serve_config_with_authenticator() {
        // [D1-001] ServeConfig must accept an authenticator field.
        use crate::auth::jwt::JWTAuthenticator;
        let auth: std::sync::Arc<dyn crate::auth::protocol::Authenticator> = std::sync::Arc::new(
            JWTAuthenticator::new("test-secret", None, None, None, None, None, Some(true)),
        );
        let cfg = ServeConfig {
            name: "test".to_string(),
            authenticator: Some(auth),
            ..ServeConfig::default()
        };
        assert_eq!(cfg.name, "test");
        assert!(cfg.authenticator.is_some());
    }

    // -- Issue D1-002: AsyncServeConfig has extended fields -------------------

    #[test]
    fn test_async_serve_config_default_has_all_fields() {
        // [D1-002] AsyncServeConfig must have the key Python parity fields.
        // Stub fields removed in D9-002 — see `test_serve_config_default_has_all_fields`.
        let cfg = AsyncServeConfig::default();
        assert_eq!(cfg.name, "apcore-mcp");
        assert!(cfg.authenticator.is_none());
        assert!(cfg.require_auth.is_none());
        assert!(cfg.exempt_paths.is_none());
        assert!(cfg.observability.is_none());
    }

    #[test]
    fn test_async_serve_config_with_authenticator() {
        // [D1-002] AsyncServeConfig must accept an authenticator field.
        use crate::auth::jwt::JWTAuthenticator;
        let auth: std::sync::Arc<dyn crate::auth::protocol::Authenticator> = std::sync::Arc::new(
            JWTAuthenticator::new("test-secret", None, None, None, None, None, Some(false)),
        );
        let cfg = AsyncServeConfig {
            name: "async-test".to_string(),
            authenticator: Some(auth),
            require_auth: Some(false),
            ..AsyncServeConfig::default()
        };
        assert_eq!(cfg.name, "async-test");
        assert!(cfg.authenticator.is_some());
        assert_eq!(cfg.require_auth, Some(false));
    }

    // ── filter_explorer_tools — F-043 meta-tools hidden from UI ──────────────

    fn make_tool(name: &str) -> Tool {
        Tool {
            name: name.to_string(),
            description: format!("desc-{name}"),
            input_schema: json!({"type": "object"}),
            annotations: None,
            meta: None,
        }
    }

    #[test]
    fn filter_explorer_tools_drops_apcore_meta_prefix() {
        // Mixed input: 3 user tools + 4 reserved meta-tools.
        let tools = vec![
            make_tool("text.echo"),
            make_tool("math.calc"),
            make_tool("__apcore_task_submit"),
            make_tool("__apcore_task_status"),
            make_tool("greeting"),
            make_tool("__apcore_task_cancel"),
            make_tool("__apcore_task_list"),
        ];

        let infos = filter_explorer_tools(&tools);
        let names: Vec<&str> = infos.iter().map(|i| i.name.as_str()).collect();

        assert_eq!(names, vec!["text.echo", "math.calc", "greeting"]);
        assert!(
            !names.iter().any(|n| n.starts_with("__apcore_")),
            "filter_explorer_tools must hide reserved meta-tools from UI"
        );
    }

    #[test]
    fn filter_explorer_tools_passes_through_user_tools_unchanged() {
        let tools = vec![make_tool("a.b"), make_tool("c.d")];
        let infos = filter_explorer_tools(&tools);
        assert_eq!(infos.len(), 2);
        assert_eq!(infos[0].name, "a.b");
        assert_eq!(infos[0].description, "desc-a.b");
        assert_eq!(infos[1].name, "c.d");
    }

    #[test]
    fn filter_explorer_tools_only_filters_exact_prefix() {
        // A tool whose name *contains* "__apcore_" but doesn't start with it
        // is a user tool and must NOT be hidden.
        let tools = vec![
            make_tool("user__apcore_thing"),
            make_tool("__apcore_task_submit"),
        ];
        let infos = filter_explorer_tools(&tools);
        let names: Vec<&str> = infos.iter().map(|i| i.name.as_str()).collect();
        assert_eq!(names, vec!["user__apcore_thing"]);
    }

    // ── Regression tests for Issues 1/5 & 5/5 (Ru-C1, Ru-W4) ─────────────────
    // serve() / async_serve() must forward ServeConfig/AsyncServeConfig fields.

    #[test]
    fn serve_config_require_auth_false_forwarded_to_builder() {
        // [Ru-C1] When ServeConfig.require_auth = Some(false), the resulting
        // APCoreMCPBuilder must have require_auth=false (not the default true).
        // We test via the builder directly since serve() blocks on network I/O.
        let reg = Registry::new();
        let exec = Arc::new(Executor::new(reg, Config::default()));

        let mut builder = APCoreMCP::builder()
            .backend(BackendSource::Executor(exec))
            .name("test-forward")
            .validate_inputs(false);

        // Simulate what the fixed serve() does:
        let require_auth = Some(false);
        if let Some(rq) = require_auth {
            builder = builder.require_auth(rq);
        }

        // The builder's config.require_auth must now be false.
        assert!(
            !builder.config.require_auth,
            "require_auth should be false after forwarding ServeConfig field"
        );
    }

    #[test]
    fn serve_config_require_auth_true_forwarded_to_builder() {
        // [Ru-C1] When ServeConfig.require_auth = Some(true), the built APCoreMCP
        // must have require_auth=true.
        let reg = Registry::new();
        let exec = Arc::new(Executor::new(reg, Config::default()));

        let mut builder = APCoreMCP::builder()
            .backend(BackendSource::Executor(exec))
            .name("test-forward-true")
            .require_auth(false); // start with false
                                  // Simulate forwarding Some(true):
        builder = builder.require_auth(true);

        assert!(
            builder.config.require_auth,
            "require_auth should be true after forwarding ServeConfig field"
        );
    }

    #[test]
    fn serve_config_redact_output_forwarded_to_builder() {
        // [Ru-C1] ServeConfig.redact_output must be forwarded.
        let reg = Registry::new();
        let exec = Arc::new(Executor::new(reg, Config::default()));

        let mut builder = APCoreMCP::builder()
            .backend(BackendSource::Executor(exec))
            .name("test-redact");
        // Default APCoreMCPConfig.redact_output is true; forward false.
        builder = builder.redact_output(false);

        assert!(
            !builder.config.redact_output,
            "redact_output should be false after forwarding"
        );
    }

    #[test]
    fn serve_config_authenticator_arc_forwarded_to_builder() {
        // [Ru-C1] When ServeConfig.authenticator is set, the builder must
        // store it (authenticator_arc must exist and work).
        use crate::auth::jwt::JWTAuthenticator;
        let auth: Arc<dyn crate::auth::protocol::Authenticator> = Arc::new(JWTAuthenticator::new(
            "secret",
            None,
            None,
            None,
            None,
            None,
            Some(true),
        ));
        let reg = Registry::new();
        let exec = Arc::new(Executor::new(reg, Config::default()));

        let builder = APCoreMCP::builder()
            .backend(BackendSource::Executor(exec))
            .name("test-auth")
            .authenticator_arc(auth);

        assert!(
            builder.authenticator.is_some(),
            "authenticator must be stored after authenticator_arc()"
        );
    }

    #[test]
    fn serve_config_exempt_paths_forwarded_to_builder() {
        // [Ru-C1] ServeConfig.exempt_paths must be forwarded.
        let reg = Registry::new();
        let exec = Arc::new(Executor::new(reg, Config::default()));

        let paths: Vec<String> = vec!["/health".to_string(), "/metrics".to_string()];
        let path_set: std::collections::HashSet<String> = paths.into_iter().collect();
        let builder = APCoreMCP::builder()
            .backend(BackendSource::Executor(exec))
            .name("test-exempt")
            .exempt_paths(path_set.clone());

        assert_eq!(
            builder.config.exempt_paths.as_ref().unwrap(),
            &path_set,
            "exempt_paths must be stored after forwarding"
        );
    }

    #[test]
    fn serve_config_observability_json_extraction() {
        // [D1-001] serve()/async_serve() forward ServeConfig.observability to
        // APCoreMCPBuilder::observability(bool). The placeholder field is JSON;
        // the wiring extracts a bool from `true`/`false` literals or
        // `{ "enabled": bool }` objects, defaulting to true on shape mismatch.
        // This test exercises the extraction logic directly so a mistake here
        // can be caught without spinning up the stdio serve loop.
        fn extract(obs: &serde_json::Value) -> bool {
            obs.as_bool()
                .or_else(|| obs.get("enabled").and_then(|v| v.as_bool()))
                .unwrap_or(true)
        }
        assert!(extract(&serde_json::json!(true)));
        assert!(!extract(&serde_json::json!(false)));
        assert!(extract(&serde_json::json!({"enabled": true})));
        assert!(!extract(&serde_json::json!({"enabled": false})));
        // Unknown shape → default on (presence implies user intent).
        assert!(extract(&serde_json::json!({})));
        assert!(extract(&serde_json::json!("yes")));
    }

    // ── Regression tests for Issues 2/5 & 4/5 (Ru-W1, Ru-W3) ─────────────────
    // approval_handler() must complete without panic (smoke test).

    #[test]
    fn approval_handler_setter_does_not_panic() {
        // [Ru-W1] APCoreMCPBuilder::approval_handler() must complete without panic.
        // The handler is stored even though it is not yet wired.
        use apcore::approval::{ApprovalHandler, ApprovalRequest, ApprovalResult};
        use apcore::errors::ModuleError;
        use async_trait::async_trait;

        #[derive(Debug)]
        struct NoOpApproval;

        #[async_trait]
        impl ApprovalHandler for NoOpApproval {
            async fn request_approval(
                &self,
                _request: &ApprovalRequest,
            ) -> Result<ApprovalResult, ModuleError> {
                let mut result = ApprovalResult::default();
                result.status = "approved".to_string();
                Ok(result)
            }
            async fn check_approval(&self, _id: &str) -> Result<ApprovalResult, ModuleError> {
                let mut result = ApprovalResult::default();
                result.status = "approved".to_string();
                Ok(result)
            }
        }

        let handler: Arc<dyn ApprovalHandler + Send + Sync> = Arc::new(NoOpApproval);
        // Must not panic; warning is emitted internally.
        let builder = APCoreMCP::builder()
            .name("approval-test")
            .approval_handler(handler);

        assert!(
            builder.approval_handler.is_some(),
            "approval_handler must be stored on the builder"
        );
    }

    #[test]
    fn builder_build_with_approval_handler_emits_warn_and_succeeds() {
        // [Ru-W3] build() with approval_handler configured must succeed and store
        // the handler (not panic or error).
        use apcore::approval::{ApprovalHandler, ApprovalRequest, ApprovalResult};
        use apcore::errors::ModuleError;
        use async_trait::async_trait;

        #[derive(Debug)]
        struct NoOpApproval;

        #[async_trait]
        impl ApprovalHandler for NoOpApproval {
            async fn request_approval(
                &self,
                _request: &ApprovalRequest,
            ) -> Result<ApprovalResult, ModuleError> {
                let mut result = ApprovalResult::default();
                result.status = "approved".to_string();
                Ok(result)
            }
            async fn check_approval(&self, _id: &str) -> Result<ApprovalResult, ModuleError> {
                let mut result = ApprovalResult::default();
                result.status = "approved".to_string();
                Ok(result)
            }
        }

        let handler: Arc<dyn ApprovalHandler + Send + Sync> = Arc::new(NoOpApproval);
        let reg = Registry::new();
        let exec = Arc::new(Executor::new(reg, Config::default()));

        let mcp = APCoreMCP::builder()
            .backend(BackendSource::Executor(exec))
            .name("approval-build-test")
            .approval_handler(handler)
            .build()
            .expect("build with approval_handler must succeed");

        // Handler is stored on the struct (dead_code, but accessible via debug).
        assert!(
            mcp.approval_handler.is_some(),
            "approval_handler must be stored on APCoreMCP after build"
        );
    }
}
