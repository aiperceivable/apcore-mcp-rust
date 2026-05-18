//! # apcore-mcp
//!
//! MCP bridge for apcore — expose apcore modules as MCP tools.
//!
//! This crate provides the bridge layer that translates apcore module
//! registries and executors into MCP-compatible tool servers.

pub mod acl_builder;
pub mod adapters;
pub mod apcore_mcp;
pub mod auth;
pub mod cli;
pub mod config;
pub mod constants;
pub mod converters;
pub mod explorer;
pub mod helpers;
/// Inspector sub-module — placeholder for future port from Python inspector/.
/// Kept crate-private until F-039 (Schema Inspector) ships per PRD; promote to
/// `pub mod` when the implementation lands.
pub(crate) mod inspector;
/// Markdown rendering for tool descriptions via apcore-toolkit's
/// `format_module(style = Markdown)`. LLMs select tools primarily from
/// the `description` string; richer Markdown packs more decision
/// signal per token than a one-line summary. apcore-toolkit 0.6+.
pub mod markdown;
pub mod middleware_builder;
pub mod server;
/// Crate version, kept in sync with Cargo.toml.
pub const VERSION: &str = env!("CARGO_PKG_VERSION");

// ---- Re-exports: core bridge ------------------------------------------------
pub use crate::apcore_mcp::{
    APCoreMCP, APCoreMCPBuilder, APCoreMCPConfig, APCoreMCPError, BackendSource,
};
pub use crate::apcore_mcp::{AsyncServeConfig, OpenAIToolsConfig, ServeConfig};
pub use crate::apcore_mcp::{AsyncServeOptions, ExplorerOptions, ServeOptions};

// ---- Re-exports: top-level convenience functions ----------------------------
pub use crate::apcore_mcp::{async_serve, serve, to_openai_tools};

// ---- Re-exports: auth -------------------------------------------------------
pub use crate::auth::jwt::{ClaimMapping, JWTAuthenticator};
// [D9-009] extract_headers is internal (pub(crate)) — not re-exported.
pub use crate::auth::middleware::{AuthMiddlewareLayer, AuthMiddlewareService, AUTH_IDENTITY};
pub use crate::auth::protocol::{Authenticator, Identity};

// ---- Re-exports: server -----------------------------------------------------
pub use crate::server::factory::MCPServerFactory;
pub use crate::server::listener::RegistryListener;
pub use crate::server::router::ExecutionRouter;
pub use crate::server::server::{MCPServer, MCPServerConfig, RegistryOrExecutor, TransportKind};
pub use crate::server::transport::TransportManager;

// ---- Re-exports: adapters ---------------------------------------------------
pub use crate::adapters::internal_error_response;
pub use crate::adapters::register_mcp_formatter;
pub use crate::adapters::AdapterError;
pub use crate::adapters::AnnotationMapper;
pub use crate::adapters::ElicitationApprovalHandler;
pub use crate::adapters::ErrorMapper;
pub use crate::adapters::McpErrorFormatter;
pub use crate::adapters::ModuleIDNormalizer;
pub use crate::adapters::SchemaConverter;

// ---- Re-exports: config bus -------------------------------------------------
pub use crate::config::{register_mcp_namespace, MCP_ENV_PREFIX, MCP_NAMESPACE};

// ---- Re-exports: converters -------------------------------------------------
pub use crate::converters::openai::{
    json_entry_to_scanned_module, ConvertOptions, ConverterError, OpenAIConverter,
};

// ---- Re-exports: helpers ----------------------------------------------------
pub use crate::helpers::{elicit, report_progress, ElicitResult, MCP_ELICIT_KEY, MCP_PROGRESS_KEY};

// ---- Re-exports: constants --------------------------------------------------
// Match top-level surface of apcore-mcp-python (REGISTRY_EVENTS, ERROR_CODES,
// MODULE_ID_PATTERN, APCORE_META_TOOL_PREFIX) and apcore-mcp-typescript
// (REGISTRY_EVENTS, ErrorCodes, MODULE_ID_PATTERN, APCORE_EVENTS,
// APCORE_META_TOOL_PREFIX).
//
// `apcore_events`: re-exported for cross-SDK parity with Python `apcore_mcp.APCORE_EVENTS`
// and TypeScript `APCORE_EVENTS`. Has no internal callers in this crate; do not delete.
pub use crate::constants::{apcore_events, ErrorCode, RegistryEvent, MODULE_ID_PATTERN};

// `APCORE_META_TOOL_PREFIX`: the `"__apcore_"` namespace reserved for
// apcore-mcp meta-tools. Re-exported under the cross-SDK canonical name
// while the internal short name `META_TOOL_PREFIX` remains for backwards
// compatibility with existing call sites inside this crate. [A-001]
pub use crate::server::async_task_bridge::META_TOOL_PREFIX as APCORE_META_TOOL_PREFIX;
