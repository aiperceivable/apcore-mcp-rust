//! Extensions example — register modules from a "plugin directory" pattern.
//!
//! This example mirrors `apcore-mcp-python/examples/extensions/` and
//! `apcore-mcp-typescript/examples/extensions/`: instead of registering
//! modules inline (as `examples/run/` does), it groups module definitions
//! into a `modules/` subdirectory and registers them via a discovery-style
//! enumeration. In Python, each `extensions/*.py` file is auto-imported by
//! the apcore Registry; in Rust, we approximate this by listing each
//! module's `register` constructor in a single sweep.
//!
//! Usage (from the project root):
//!
//!     cargo run --example extensions
//!
//! Then open http://127.0.0.1:8000/explorer in your browser.
//!
//! Available tools:
//!   - text.echo
//!   - math.calc
//!   - greeting

mod modules;

use std::sync::Arc;

use apcore::config::Config;
use apcore::executor::Executor;
use apcore::registry::registry::Registry;
use apcore_mcp::{APCoreMCP, ExplorerOptions, ServeOptions};

fn main() -> Result<(), Box<dyn std::error::Error>> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .init();

    // 1. Create a registry. In Python, `extensions_dir` triggers filesystem
    //    discovery; here we explicitly enumerate each module's register fn
    //    to keep the example dependency-light and platform-agnostic.
    let registry = Registry::new();
    modules::register_all(&registry)?;
    println!(
        "Registered {} modules from extensions/",
        registry.list(None, None, None).len()
    );

    // 2. Build an executor over the registry.
    let executor = Arc::new(Executor::new(
        Arc::new(registry),
        Arc::new(Config::default()),
    ));

    // 3. Launch MCP server with Explorer UI.
    let mcp = APCoreMCP::builder()
        .backend(executor)
        .name("apcore-mcp-extensions-demo")
        .transport("streamable-http")
        .host("127.0.0.1")
        .port(8000)
        .include_explorer(true)
        .allow_execute(true)
        .explorer_title("APCore Extensions Demo")
        .build()?;

    mcp.serve_with_options(ServeOptions {
        explorer: ExplorerOptions::default(),
        ..Default::default()
    })?;
    Ok(())
}
