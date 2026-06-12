//! Server sub-module — MCP server, factory, router, transport, and listener.

// TODO(D8-004): TypeScript canonical layout has server/context.ts, server/observability.ts,
// server/trace-context.ts. Add equivalents here (or document the consolidated layout) — see
// audit D8-004.

pub mod approval_bridge;
pub mod async_task_bridge;
pub mod factory;
pub mod listener;
pub mod router;
#[allow(clippy::module_inception)]
pub mod server;
pub mod transport;
pub mod types;

// ---- Re-exports: key public types ------------------------------------------
pub use self::listener::RegistryListener;
pub use self::server::{MCPServer, MCPServerConfig, RegistryOrExecutor, TransportKind};
