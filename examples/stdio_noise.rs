//! Adversarial stdio example: ordinary application output must reach stderr.
//! Run `cargo run --example stdio_noise` and send newline-delimited MCP JSON-RPC.

use std::collections::HashMap;
use std::sync::Arc;

use apcore::config::Config;
use apcore::context::Context;
use apcore::errors::{ErrorCode, ModuleError};
use apcore::events::{ApCoreEvent, EventSubscriber, StdoutSubscriber};
use apcore::executor::Executor;
use apcore::module::Module;
use apcore::registry::registry::{ModuleDescriptor, Registry};
use apcore_mcp::{APCoreMCP, ServeOptions};
use async_trait::async_trait;
use serde_json::{json, Value};

struct NoisyModule {
    fail: bool,
    wait: bool,
    cooperative: bool,
    started: Arc<tokio::sync::Notify>,
}

struct CancellationCleanup;
impl Drop for CancellationCleanup {
    fn drop(&mut self) {
        println!("stdio-async-cleanup");
    }
}

struct NoisyApprovalStore;
type StoreError = Box<dyn std::error::Error + Send + Sync>;

#[async_trait]
impl apcore_mcp::approval_store::ApprovalStore for NoisyApprovalStore {
    async fn save_pending(
        &self,
        _id: &str,
        _module: &str,
        _arguments: &Value,
    ) -> Result<(), StoreError> {
        Ok(())
    }
    async fn get_result(
        &self,
        _id: &str,
    ) -> Result<Option<apcore_mcp::approval_store::ApprovalRecord>, StoreError> {
        Ok(None)
    }
    async fn resolve(
        &self,
        _id: &str,
        _approved: bool,
        _reason: Option<String>,
    ) -> Result<bool, StoreError> {
        Ok(false)
    }
    fn start_sweep(&self) -> Option<tokio::task::JoinHandle<()>> {
        struct SweepCleanup;
        impl Drop for SweepCleanup {
            fn drop(&mut self) {
                println!("stdio-sweep-cleanup");
            }
        }
        Some(tokio::spawn(async {
            let _cleanup = SweepCleanup;
            eprintln!("stdio-sweep-started");
            std::future::pending::<()>().await;
        }))
    }
}

#[async_trait]
impl Module for NoisyModule {
    fn input_schema(&self) -> Value {
        json!({"type": "object"})
    }
    fn output_schema(&self) -> Value {
        json!({"type": "object"})
    }
    fn description(&self) -> &str {
        "Exercise stdout isolation during module execution"
    }
    async fn execute(
        &self,
        _inputs: Value,
        context: &Context<Value>,
    ) -> Result<Value, ModuleError> {
        if self.wait {
            let _cleanup = CancellationCleanup;
            eprintln!("stdio-async-started");
            self.started.notify_one();
            if self.cooperative {
                while !context
                    .cancel_token
                    .as_ref()
                    .is_some_and(|token| token.is_cancelled())
                {
                    tokio::time::sleep(std::time::Duration::from_millis(2)).await;
                }
                tokio::time::sleep(std::time::Duration::from_millis(30)).await;
                return Ok(json!({"cleaned": true}));
            } else {
                std::future::pending::<()>().await;
            }
        }
        println!("stdio-business-print");
        tracing::warn!("stdio-runtime-tracing");
        StdoutSubscriber::new()
            .on_event(&ApCoreEvent::new("stdio-subscriber", json!({})))
            .await?;
        // A separate task does not inherit task-local ownership, but its stdout
        // must still be redirected by the process-wide descriptor boundary.
        tokio::spawn(async {
            println!("stdio-background-print");
        })
        .await
        .unwrap();
        if self.fail {
            Err(ModuleError::new(
                ErrorCode::GeneralInternalError,
                "stdio-error-tool",
            ))
        } else {
            Ok(json!({"isolated": true}))
        }
    }
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    // Intentionally use tracing's stdout default to exercise an embedding
    // application's subscriber, independently of the CLI's stderr default.
    tracing_subscriber::fmt().with_ansi(false).init();
    let registry = Registry::new();
    let started = Arc::new(tokio::sync::Notify::new());
    let mode = std::env::var("APCORE_STDIO_FIXTURE_MODE").unwrap_or_default();
    for (module_id, fail, wait, cooperative) in [
        ("stdio_noise", false, false, false),
        ("stdio_error", true, false, false),
        ("stdio_wait", false, true, false),
        ("stdio_cooperative", false, true, true),
    ] {
        let module = NoisyModule {
            fail,
            wait,
            cooperative,
            started: started.clone(),
        };
        let descriptor = ModuleDescriptor {
            module_id: module_id.into(),
            name: None,
            description: module.description().into(),
            documentation: None,
            input_schema: module.input_schema(),
            output_schema: module.output_schema(),
            version: "1.0.0".into(),
            tags: vec![],
            annotations: None,
            examples: vec![],
            metadata: if wait && !cooperative {
                HashMap::from([("async".into(), json!(true))])
            } else {
                HashMap::new()
            },
            display: None,
            sunset_date: None,
            dependencies: vec![],
            enabled: true,
        };
        registry.register(module_id, Box::new(module), descriptor)?;
    }
    let mut config = Config::default();
    if mode == "timeout" {
        config.set("executor.default_timeout", json!(20));
    }
    let executor = Arc::new(Executor::new(Arc::new(registry), Arc::new(config)));
    let mut builder = APCoreMCP::builder().backend(executor).transport("stdio");
    if mode == "sweep" {
        builder = builder.approval_store(Arc::new(NoisyApprovalStore));
    }
    let server = builder.build()?;
    let options = ServeOptions {
        on_startup: Some(Box::new(|| println!("stdio-startup-callback"))),
        on_shutdown: Some(Box::new(|| println!("stdio-shutdown-callback"))),
        ..Default::default()
    };
    // Keeping the caller's runtime alive after serve returns exposes detached
    // SDK cleanup that a short-lived CLI process could otherwise hide.
    tokio::runtime::Runtime::new()?.block_on(async {
        if mode == "cancel" {
            let mut serving = Box::pin(server.serve_with_options_async(options));
            tokio::select! {
                result = &mut serving => { result?; }
                _ = started.notified() => {}
            }
            drop(serving);
        } else {
            server.serve_with_options_async(options).await?;
        }
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        if mode == "cancel" {
            println!("stdio-restored-after-cancel");
        }
        Ok::<_, apcore_mcp::APCoreMCPError>(())
    })?;
    Ok(())
}
