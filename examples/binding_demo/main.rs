//! Binding demo — load `.binding.yaml` files via apcore-toolkit's
//! BindingLoader and expose the bound functions as MCP tools without
//! changing `myapp.rs` (which has no apcore dependencies).
//!
//! Mirrors `apcore-mcp-python/examples/binding_demo/`. Rust differs from
//! Python in that there is no reflection — each binding's `target`
//! string still needs a hand-written Module impl that delegates to the
//! plain function. The BindingLoader is used here for metadata
//! (description/tags/annotations from YAML) rather than for runtime
//! dispatch.
//!
//! Usage (from the project root):
//!
//!     cargo run --example binding_demo
//!
//! Then open http://127.0.0.1:8000/explorer in your browser.

mod myapp;

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;

use async_trait::async_trait;
use serde_json::{json, Value};

use apcore::config::Config;
use apcore::context::Context;
use apcore::errors::{ErrorCode, ModuleError};
use apcore::executor::Executor;
use apcore::module::{Module, ModuleAnnotations};
use apcore::registry::registry::{ModuleDescriptor, Registry};
use apcore_mcp::{APCoreMCP, ExplorerOptions, ServeOptions};
use apcore_toolkit::BindingLoader;

// ---------------------------------------------------------------------------
// Module wrappers — delegate to plain `myapp` functions
// ---------------------------------------------------------------------------

struct ConvertTemperatureModule;

#[async_trait]
impl Module for ConvertTemperatureModule {
    fn input_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "value": {"type": "number"},
                "from_unit": {"type": "string", "default": "celsius"},
                "to_unit": {"type": "string", "default": "fahrenheit"}
            },
            "required": ["value"]
        })
    }
    fn output_schema(&self) -> Value {
        json!({"type": "object"})
    }
    fn description(&self) -> &str {
        "Convert temperature between Celsius, Fahrenheit, and Kelvin"
    }
    async fn execute(&self, inputs: Value, _ctx: &Context<Value>) -> Result<Value, ModuleError> {
        let value = inputs
            .get("value")
            .and_then(|v| v.as_f64())
            .ok_or_else(|| ModuleError::new(ErrorCode::GeneralInvalidInput, "missing 'value'"))?;
        let from_unit = inputs
            .get("from_unit")
            .and_then(|v| v.as_str())
            .unwrap_or("celsius");
        let to_unit = inputs
            .get("to_unit")
            .and_then(|v| v.as_str())
            .unwrap_or("fahrenheit");
        myapp::convert_temperature(value, from_unit, to_unit)
            .map(|m| serde_json::to_value(m).unwrap_or(Value::Null))
            .map_err(|e| ModuleError::new(ErrorCode::GeneralInvalidInput, e))
    }
}

struct WordCountModule;

#[async_trait]
impl Module for WordCountModule {
    fn input_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {"text": {"type": "string"}},
            "required": ["text"]
        })
    }
    fn output_schema(&self) -> Value {
        json!({"type": "object"})
    }
    fn description(&self) -> &str {
        "Count words, characters, and lines in a text string"
    }
    async fn execute(&self, inputs: Value, _ctx: &Context<Value>) -> Result<Value, ModuleError> {
        let text = inputs
            .get("text")
            .and_then(|v| v.as_str())
            .ok_or_else(|| ModuleError::new(ErrorCode::GeneralInvalidInput, "missing 'text'"))?;
        let counts = myapp::word_count(text);
        Ok(serde_json::to_value(counts).unwrap_or(Value::Null))
    }
}

// ---------------------------------------------------------------------------
// Bridge: ScannedModule → Module trait impl
// ---------------------------------------------------------------------------

fn module_for_target(target: &str) -> Option<Box<dyn Module>> {
    match target {
        "myapp:convert_temperature" => Some(Box::new(ConvertTemperatureModule)),
        "myapp:word_count" => Some(Box::new(WordCountModule)),
        _ => None,
    }
}

// ---------------------------------------------------------------------------
// main
// ---------------------------------------------------------------------------

fn main() -> Result<(), Box<dyn std::error::Error>> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .init();

    // 1. Load .binding.yaml files via apcore-toolkit's BindingLoader.
    //    The loader gives us `ScannedModule` records (pure data); we still
    //    need to map each `target` string to a concrete Module impl since
    //    Rust has no reflection.
    let binding_dir: PathBuf =
        PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("examples/binding_demo/extensions");
    let loader = BindingLoader::new();
    let scanned = loader.load(
        &binding_dir,
        /* strict */ false,
        /* recursive */ false,
    )?;
    println!(
        "Loaded {} binding(s) from {}",
        scanned.len(),
        binding_dir.display()
    );

    // 2. Build the registry. For each scanned binding, look up the Module
    //    impl by `target` and register with the metadata from the YAML.
    let registry = Registry::new();
    for sm in scanned {
        let module = match module_for_target(&sm.target) {
            Some(m) => m,
            None => {
                eprintln!(
                    "warn: no Module impl registered for target {:?}; skipping",
                    sm.target
                );
                continue;
            }
        };
        let descriptor = ModuleDescriptor {
            module_id: sm.module_id.clone(),
            name: None,
            description: sm.description.clone(),
            documentation: None,
            input_schema: module.input_schema(),
            output_schema: module.output_schema(),
            version: sm.version.clone(),
            tags: sm.tags,
            annotations: Some(ModuleAnnotations {
                readonly: sm.annotations.as_ref().is_some_and(|a| a.readonly),
                destructive: sm.annotations.as_ref().is_some_and(|a| a.destructive),
                idempotent: sm.annotations.as_ref().is_some_and(|a| a.idempotent),
                open_world: sm.annotations.as_ref().is_some_and(|a| a.open_world),
                ..Default::default()
            }),
            examples: vec![],
            metadata: HashMap::new(),
            display: None,
            sunset_date: None,
            dependencies: vec![],
            enabled: true,
        };
        registry.register(&sm.module_id, module, descriptor)?;
    }
    println!(
        "Registered {} modules from binding files",
        registry.list(None, None, None).len()
    );

    // 3. Build executor + launch server with Explorer.
    let executor = Arc::new(Executor::new(
        Arc::new(registry),
        Arc::new(Config::default()),
    ));
    let mcp = APCoreMCP::builder()
        .backend(executor)
        .name("apcore-mcp-binding-demo")
        .transport("streamable-http")
        .host("127.0.0.1")
        .port(8000)
        .include_explorer(true)
        .allow_execute(true)
        .explorer_title("APCore Binding Demo")
        .build()?;

    mcp.serve_with_options(ServeOptions {
        explorer: ExplorerOptions::default(),
        ..Default::default()
    })?;
    Ok(())
}
