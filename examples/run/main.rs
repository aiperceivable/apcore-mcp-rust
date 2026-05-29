//! Launch MCP server with example extension modules.
//!
//! Usage (from the project root):
//!
//!     cargo run --example run
//!
//! Enable JWT authentication by setting APCORE_JWT_SECRET:
//!
//!     APCORE_JWT_SECRET=my-secret cargo run --example run
//!
//! Then open http://127.0.0.1:8000/explorer in your browser.
//!
//! Test with curl:
//!
//!     curl http://localhost:8000/health                              # 200 (exempt)
//!     curl -X POST http://localhost:8000/mcp ...                     # 401 (no token)
//!     curl -H "Authorization: Bearer <token>" -X POST localhost:8000/mcp  # 200
//!
//! Available tools:
//!   - text.echo     — echo text back, optionally uppercase
//!   - math.calc     — basic arithmetic (add, sub, mul, div)
//!   - greeting      — personalized greeting in different styles

mod modules;

use std::sync::Arc;

use apcore::config::Config;
use apcore::executor::Executor;
use apcore::module::{Module, ModuleAnnotations};
use apcore::registry::registry::{ModuleDescriptor, Registry};
use apcore_mcp::{APCoreMCP, ExplorerOptions, JWTAuthenticator, ServeOptions};

fn main() -> Result<(), Box<dyn std::error::Error>> {
    // Initialize tracing (respects RUST_LOG env var)
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .init();

    // 1. Create a registry and register example modules.
    let registry = Registry::new();

    registry.register(
        "text.echo",
        Box::new(modules::TextEcho),
        ModuleDescriptor {
            module_id: "text.echo".into(),
            name: None,
            description: "Echo text back".into(),
            documentation: None,
            input_schema: modules::TextEcho.input_schema(),
            output_schema: modules::TextEcho.output_schema(),
            version: "1.0.0".into(),
            tags: vec!["text".into(), "utility".into()],
            annotations: Some(ModuleAnnotations {
                readonly: true,
                idempotent: true,
                open_world: false,
                ..Default::default()
            }),
            examples: vec![],
            metadata: std::collections::HashMap::new(),
            display: None,
            sunset_date: None,
            dependencies: vec![],
            enabled: true,
        },
    )?;

    registry.register(
        "math.calc",
        Box::new(modules::MathCalc),
        ModuleDescriptor {
            module_id: "math.calc".into(),
            name: None,
            description: "Calculate math expressions".into(),
            documentation: None,
            input_schema: modules::MathCalc.input_schema(),
            output_schema: modules::MathCalc.output_schema(),
            version: "1.0.0".into(),
            tags: vec!["math".into(), "utility".into()],
            annotations: Some(ModuleAnnotations {
                readonly: true,
                idempotent: true,
                open_world: false,
                ..Default::default()
            }),
            examples: vec![],
            metadata: std::collections::HashMap::new(),
            display: None,
            sunset_date: None,
            dependencies: vec![],
            enabled: true,
        },
    )?;

    registry.register(
        "greeting",
        Box::new(modules::Greeting),
        ModuleDescriptor {
            module_id: "greeting".into(),
            name: None,
            description: "Generate a greeting".into(),
            documentation: None,
            input_schema: modules::Greeting.input_schema(),
            output_schema: modules::Greeting.output_schema(),
            version: "1.0.0".into(),
            tags: vec!["text".into(), "fun".into()],
            annotations: Some(ModuleAnnotations {
                readonly: true,
                open_world: false,
                ..Default::default()
            }),
            examples: vec![],
            metadata: std::collections::HashMap::new(),
            display: None,
            sunset_date: None,
            dependencies: vec![],
            enabled: true,
        },
    )?;

    tracing::info!(
        "Registered {} modules",
        registry.list(None, None, None).len()
    );

    // 2. Create executor from registry.
    let executor = Arc::new(Executor::new(registry, Config::default()));

    // 3. Build JWT authenticator if APCORE_JWT_SECRET is set.
    let jwt_secret = std::env::var("APCORE_JWT_SECRET").ok();
    let mut builder = APCoreMCP::builder()
        .backend(executor)
        .name("apcore-mcp-examples")
        .transport("streamable-http")
        .host("127.0.0.1")
        .port(8000)
        .include_explorer(true)
        .allow_execute(true)
        .validate_inputs(true)
        .explorer_title("APCore MCP Examples Explorer")
        .explorer_project_name("apcore-mcp")
        .explorer_project_url("https://github.com/aiperceivable/apcore-mcp-rust");

    if let Some(ref secret) = jwt_secret {
        let authenticator = JWTAuthenticator::new(secret, None, None, None, None, None, None);
        builder = builder.authenticator(authenticator).require_auth(true);
        tracing::info!("JWT authentication:  enabled (HS256)");

        // Generate a sample token for testing.
        let header = jsonwebtoken::Header::default();
        let claims = serde_json::json!({
            "sub": "demo-user",
            "type": "user",
            "roles": ["admin"],
            "exp": chrono::Utc::now().timestamp() + 3600
        });
        let token = jsonwebtoken::encode(
            &header,
            &claims,
            &jsonwebtoken::EncodingKey::from_secret(secret.as_bytes()),
        )
        .expect("failed to encode sample JWT");
        tracing::info!("Sample token:        {token}");
    } else {
        builder = builder.require_auth(false);
        tracing::info!("JWT authentication:  disabled (set APCORE_JWT_SECRET to enable)");
    }

    let mcp = builder.build()?;

    tracing::info!("Registered tools: {:?}", mcp.tools());
    tracing::info!("Explorer UI:      http://127.0.0.1:8000/explorer");

    // 4. Serve (blocks the current thread).
    mcp.serve_with_options(ServeOptions {
        explorer: ExplorerOptions {
            explorer: true,
            allow_execute: true,
            explorer_prefix: "/explorer".to_string(),
            explorer_title: "APCore MCP Examples Explorer".to_string(),
            explorer_project_name: Some("apcore-mcp".to_string()),
            explorer_project_url: Some(
                "https://github.com/aiperceivable/apcore-mcp-rust".to_string(),
            ),
        },
        on_startup: Some(Box::new(|| {
            println!("\n  MCP server ready at http://127.0.0.1:8000");
            println!("  Explorer UI at     http://127.0.0.1:8000/explorer\n");
        })),
        on_shutdown: Some(Box::new(|| {
            println!("MCP server shut down.");
        })),
        dynamic: false,
    })?;

    Ok(())
}
