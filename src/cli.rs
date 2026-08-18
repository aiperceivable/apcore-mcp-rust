//! CLI entry point for the apcore-mcp binary.
//!
//! Uses clap to parse command-line arguments and starts the MCP server.

use std::collections::HashSet;
use std::path::PathBuf;
use std::sync::Arc;

use clap::{ArgAction, Parser, ValueEnum};
use tracing_subscriber::{fmt, EnvFilter};

// ── Enums ──────────────────────────────────────────────────────────────

/// Transport protocol for the MCP server.
#[derive(Clone, Debug, PartialEq, ValueEnum)]
pub enum Transport {
    Stdio,
    #[value(name = "streamable-http")]
    StreamableHttp,
    Sse,
}

/// Pipeline execution strategy preset.
#[derive(Clone, Debug, PartialEq, ValueEnum)]
pub enum StrategyPreset {
    Standard,
    Internal,
    Testing,
    Performance,
    Minimal,
}

impl StrategyPreset {
    /// Return the lowercase string representation for the apcore executor.
    pub fn as_str(&self) -> &'static str {
        match self {
            StrategyPreset::Standard => "standard",
            StrategyPreset::Internal => "internal",
            StrategyPreset::Testing => "testing",
            StrategyPreset::Performance => "performance",
            StrategyPreset::Minimal => "minimal",
        }
    }
}

/// Approval handler mode.
#[derive(Clone, Debug, PartialEq, ValueEnum)]
pub enum ApprovalMode {
    Elicit,
    #[value(name = "auto-approve")]
    AutoApprove,
    #[value(name = "always-deny")]
    AlwaysDeny,
    Off,
}

/// Logging level (maps to tracing levels).
#[derive(Clone, Debug, PartialEq, ValueEnum)]
pub enum LogLevel {
    #[value(name = "DEBUG")]
    Debug,
    #[value(name = "INFO")]
    Info,
    #[value(name = "WARNING")]
    Warning,
    #[value(name = "ERROR")]
    Error,
}

impl LogLevel {
    /// Return the tracing-compatible filter string.
    pub fn to_filter_str(&self) -> &'static str {
        match self {
            LogLevel::Debug => "debug",
            LogLevel::Info => "info",
            LogLevel::Warning => "warn",
            LogLevel::Error => "error",
        }
    }

    /// Map to `tracing::level_filters::LevelFilter`.
    pub fn to_level_filter(&self) -> tracing::level_filters::LevelFilter {
        match self {
            LogLevel::Debug => tracing::level_filters::LevelFilter::DEBUG,
            LogLevel::Info => tracing::level_filters::LevelFilter::INFO,
            LogLevel::Warning => tracing::level_filters::LevelFilter::WARN,
            LogLevel::Error => tracing::level_filters::LevelFilter::ERROR,
        }
    }
}

// ── Tracing initialisation ─────────────────────────────────────────────

/// Initialise the global tracing subscriber with the given log level.
///
/// Uses `EnvFilter` so that `RUST_LOG` can override the CLI-provided level.
/// This function should be called exactly once at startup.
pub fn init_tracing(level: &LogLevel) {
    let filter = EnvFilter::new(level.to_filter_str());

    fmt()
        .with_env_filter(filter)
        .with_target(true)
        .with_thread_ids(false)
        .with_file(false)
        .init();
}

// ── CLI args ───────────────────────────────────────────────────────────

/// Command-line arguments for the apcore-mcp server.
#[derive(Parser, Debug)]
#[command(
    name = "apcore-mcp",
    about = "Launch an MCP server that exposes apcore modules as tools."
)]
pub struct CliArgs {
    /// Path to apcore extensions directory.
    #[arg(long)]
    pub extensions_dir: PathBuf,

    /// Transport type.
    #[arg(long, value_enum, default_value_t = Transport::Stdio)]
    pub transport: Transport,

    /// Host address for HTTP-based transports.
    #[arg(long, default_value = "127.0.0.1")]
    pub host: String,

    /// Port for HTTP-based transports (1-65535).
    #[arg(long, default_value_t = 8000, value_parser = clap::value_parser!(u16).range(1..))]
    pub port: u16,

    /// MCP server name (max 255 characters).
    #[arg(long, default_value = "apcore-mcp")]
    pub name: String,

    /// MCP server version (default: package version).
    #[arg(long)]
    pub version: Option<String>,

    /// Logging level.
    #[arg(long, value_enum, default_value_t = LogLevel::Info)]
    pub log_level: LogLevel,

    /// Enable the browser-based Tool Explorer UI.
    #[arg(long, default_value_t = false)]
    pub explorer: bool,

    /// URL prefix for the explorer UI.
    #[arg(long, default_value = "/explorer")]
    pub explorer_prefix: String,

    /// Allow tool execution from the explorer UI.
    #[arg(long, default_value_t = false)]
    pub allow_execute: bool,

    /// Page title shown in the explorer browser tab and heading.
    #[arg(long, default_value = "MCP Tool Explorer")]
    pub explorer_title: String,

    /// Optional project name shown in the explorer footer.
    #[arg(long)]
    pub explorer_project_name: Option<String>,

    /// Optional project URL linked in the explorer footer.
    #[arg(long)]
    pub explorer_project_url: Option<String>,

    /// JWT secret key for Bearer token authentication.
    #[arg(long, env = "APCORE_JWT_SECRET")]
    pub jwt_secret: Option<String>,

    /// Path to PEM key file for JWT verification.
    #[arg(long)]
    pub jwt_key_file: Option<PathBuf>,

    /// JWT algorithm.
    #[arg(long, default_value = "HS256")]
    pub jwt_algorithm: String,

    /// Expected JWT audience claim.
    #[arg(long)]
    pub jwt_audience: Option<String>,

    /// Expected JWT issuer claim.
    #[arg(long)]
    pub jwt_issuer: Option<String>,

    /// Require JWT authentication (use --no-jwt-require-auth to disable).
    #[arg(long, default_value_t = true, action = ArgAction::Set)]
    pub jwt_require_auth: bool,

    /// Comma-separated paths exempt from auth.
    #[arg(long)]
    pub exempt_paths: Option<String>,

    /// Approval handler mode.
    #[arg(long, value_enum, default_value_t = ApprovalMode::Off)]
    pub approval: ApprovalMode,

    /// Pipeline execution strategy.
    #[arg(long, value_enum, help = "Pipeline execution strategy")]
    pub strategy: Option<StrategyPreset>,

    /// Built-in output format.
    #[arg(long, value_enum, default_value_t = crate::server::router::OutputFormat::Json)]
    pub output_format: crate::server::router::OutputFormat,

    /// Enable the built-in observability stack (auto-wires
    /// `MetricsMiddleware` and `UsageMiddleware`, and exposes `/metrics`
    /// (Prometheus) and `/usage` (JSON) endpoints on HTTP transports).
    #[arg(long, default_value_t = false)]
    pub observability: bool,
}

// ── Error type ────────────────────────────────────────────────────────

/// Errors produced by CLI argument validation or server startup.
#[derive(Debug, thiserror::Error)]
pub enum CliError {
    /// Invalid argument values (exit code 1).
    #[error("{0}")]
    InvalidArgs(String),
    /// Server startup failure (exit code 2).
    #[error("{0}")]
    StartupFailure(String),
}

impl CliError {
    /// Return the process exit code for this error category.
    pub fn exit_code(&self) -> i32 {
        match self {
            CliError::InvalidArgs(_) => 1,
            CliError::StartupFailure(_) => 2,
        }
    }
}

// ── Helper functions ──────────────────────────────────────────────────

/// Resolve the JWT key using the priority chain: file > arg > env.
fn resolve_jwt_key(args: &CliArgs) -> Result<Option<String>, CliError> {
    if let Some(ref path) = args.jwt_key_file {
        if !path.exists() {
            return Err(CliError::InvalidArgs(format!(
                "--jwt-key-file '{}' does not exist",
                path.display()
            )));
        }
        let contents = std::fs::read_to_string(path).map_err(|e| {
            CliError::InvalidArgs(format!(
                "failed to read --jwt-key-file '{}': {e}",
                path.display()
            ))
        })?;
        Ok(Some(contents.trim().to_string()))
    } else if let Some(ref secret) = args.jwt_secret {
        Ok(Some(secret.clone()))
    } else {
        Ok(std::env::var("APCORE_JWT_SECRET").ok())
    }
}

/// Split a comma-separated string into a set of trimmed path strings.
fn parse_exempt_paths(s: &str) -> HashSet<String> {
    s.split(',')
        .map(|p| p.trim().to_string())
        .filter(|p| !p.is_empty())
        .collect()
}

/// Parse a JWT algorithm string into a `jsonwebtoken::Algorithm`.
///
/// Falls back to HS256 for unrecognized values (with a warning).
fn parse_jwt_algorithm(alg: &str) -> jsonwebtoken::Algorithm {
    use jsonwebtoken::Algorithm;
    match alg.to_uppercase().as_str() {
        "HS256" => Algorithm::HS256,
        "HS384" => Algorithm::HS384,
        "HS512" => Algorithm::HS512,
        "RS256" => Algorithm::RS256,
        "RS384" => Algorithm::RS384,
        "RS512" => Algorithm::RS512,
        "ES256" => Algorithm::ES256,
        "ES384" => Algorithm::ES384,
        "PS256" => Algorithm::PS256,
        "PS384" => Algorithm::PS384,
        "PS512" => Algorithm::PS512,
        "EDDSA" | "EdDSA" => Algorithm::EdDSA,
        other => {
            tracing::warn!("Unknown JWT algorithm '{other}', falling back to HS256");
            Algorithm::HS256
        }
    }
}

/// Validate CLI arguments beyond what clap enforces.
fn validate_args(args: &CliArgs) -> Result<(), CliError> {
    if !args.extensions_dir.exists() {
        return Err(CliError::InvalidArgs(format!(
            "--extensions-dir '{}' does not exist",
            args.extensions_dir.display()
        )));
    }
    if !args.extensions_dir.is_dir() {
        return Err(CliError::InvalidArgs(format!(
            "--extensions-dir '{}' is not a directory",
            args.extensions_dir.display()
        )));
    }
    // CHARACTERS, not UTF-8 bytes — same counting unit as
    // `APCoreMCPBuilder::build` and `MCPServerFactory::create_server`, and as
    // Python (code points) and TypeScript (UTF-16 units). [A-D-FA-3]
    let name_len = args.name.chars().count();
    if name_len > 255 {
        return Err(CliError::InvalidArgs(format!(
            "--name must be at most 255 characters, got {name_len}"
        )));
    }
    Ok(())
}

/// Map a [`Transport`] enum value to the string expected by APCoreMCP.
fn transport_to_str(t: &Transport) -> &'static str {
    match t {
        Transport::Stdio => "stdio",
        Transport::StreamableHttp => "streamable-http",
        Transport::Sse => "sse",
    }
}

// ── run ───────────────────────────────────────────────────────────────

/// Run the CLI application.
///
/// Parses arguments, initialises tracing, validates inputs, resolves
/// JWT authentication, builds the [`APCoreMCP`](crate::APCoreMCP) instance,
/// and starts the server. Returns `CliError` on failure.
pub async fn run() -> Result<(), CliError> {
    let args = CliArgs::parse();

    // Initialise tracing first so all subsequent log calls work.
    init_tracing(&args.log_level);

    // Validate arguments beyond clap constraints.
    validate_args(&args)?;

    // Resolve JWT key (file > arg > env).
    let jwt_key = resolve_jwt_key(&args)?;

    // Build authenticator if a key was resolved.
    let authenticator = jwt_key.map(|key| {
        tracing::info!(
            "JWT authentication enabled (algorithm={})",
            args.jwt_algorithm
        );
        let algorithm = parse_jwt_algorithm(&args.jwt_algorithm);
        crate::auth::jwt::JWTAuthenticator::new(
            &key,
            Some(vec![algorithm]),
            args.jwt_audience.clone(),
            args.jwt_issuer.clone(),
            None,
            None,
            Some(args.jwt_require_auth),
        )
    });

    // Parse exempt paths.
    let exempt_paths = args.exempt_paths.as_deref().map(parse_exempt_paths);

    // Build approval handler.
    let approval_handler: Option<Arc<dyn apcore::approval::ApprovalHandler>> = match args.approval {
        ApprovalMode::Elicit => {
            tracing::info!("Approval handler: elicit (MCP elicitation)");
            Some(Arc::new(
                crate::adapters::approval::ElicitationApprovalHandler::new(None),
            ))
        }
        ApprovalMode::AutoApprove => {
            tracing::info!("Approval handler: auto-approve (dev/testing)");
            Some(Arc::new(apcore::approval::AutoApproveHandler))
        }
        ApprovalMode::AlwaysDeny => {
            tracing::info!("Approval handler: always-deny (enforcement)");
            Some(Arc::new(apcore::approval::AlwaysDenyHandler))
        }
        ApprovalMode::Off => None,
    };

    // Resolve server version.
    let version = args.version.unwrap_or_else(|| crate::VERSION.to_string());

    // Build APCoreMCP via builder pattern.
    let mut builder = crate::APCoreMCPBuilder::default()
        .backend(args.extensions_dir)
        .name(&args.name)
        .version(&version)
        .transport(transport_to_str(&args.transport))
        .host(&args.host)
        .port(args.port)
        .require_auth(args.jwt_require_auth);

    if let Some(auth) = authenticator {
        builder = builder.authenticator(auth);
    }

    if let Some(paths) = exempt_paths {
        builder = builder.exempt_paths(paths);
    }

    if let Some(handler) = approval_handler {
        builder = builder.approval_handler(handler);
    }

    builder = builder
        .include_explorer(args.explorer)
        .path_prefix(&args.explorer_prefix)
        .explorer_title(&args.explorer_title)
        .allow_execute(args.allow_execute);

    if let Some(ref name) = args.explorer_project_name {
        builder = builder.explorer_project_name(name);
    }

    if let Some(ref url) = args.explorer_project_url {
        builder = builder.explorer_project_url(url);
    }

    if let Some(ref strategy) = args.strategy {
        builder = builder.strategy(strategy.as_str());
    }

    builder = builder.output_format(args.output_format);

    if args.observability {
        builder = builder.observability(true);
    }

    let mcp = builder
        .build()
        .map_err(|e| CliError::StartupFailure(e.to_string()))?;

    // Start the server on the ambient runtime (runs until shutdown).
    // `serve()` would construct a second Tokio runtime, which panics because
    // the binary already runs under `#[tokio::main]`. [B-RS-1]
    mcp.serve_async()
        .await
        .map_err(|e| CliError::StartupFailure(e.to_string()))?;

    Ok(())
}

// ── Tests ──────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use clap::ValueEnum;
    use std::io::Write;
    use tempfile::NamedTempFile;

    // ── Transport enum tests ───────────────────────────────────────

    #[test]
    fn transport_from_str_stdio() {
        let val = Transport::from_str("stdio", true).unwrap();
        assert_eq!(val, Transport::Stdio);
    }

    #[test]
    fn transport_from_str_streamable_http() {
        let val = Transport::from_str("streamable-http", true).unwrap();
        assert_eq!(val, Transport::StreamableHttp);
    }

    #[test]
    fn transport_from_str_sse() {
        let val = Transport::from_str("sse", true).unwrap();
        assert_eq!(val, Transport::Sse);
    }

    #[test]
    fn transport_invalid_value() {
        let result = Transport::from_str("http", true);
        assert!(result.is_err());
    }

    // ── ApprovalMode enum tests ────────────────────────────────────

    #[test]
    fn approval_mode_from_str_elicit() {
        let val = ApprovalMode::from_str("elicit", true).unwrap();
        assert_eq!(val, ApprovalMode::Elicit);
    }

    #[test]
    fn approval_mode_from_str_auto_approve() {
        let val = ApprovalMode::from_str("auto-approve", true).unwrap();
        assert_eq!(val, ApprovalMode::AutoApprove);
    }

    #[test]
    fn approval_mode_from_str_always_deny() {
        let val = ApprovalMode::from_str("always-deny", true).unwrap();
        assert_eq!(val, ApprovalMode::AlwaysDeny);
    }

    #[test]
    fn approval_mode_from_str_off() {
        let val = ApprovalMode::from_str("off", true).unwrap();
        assert_eq!(val, ApprovalMode::Off);
    }

    #[test]
    fn approval_mode_invalid_value() {
        let result = ApprovalMode::from_str("approve", true);
        assert!(result.is_err());
    }

    // ── LogLevel enum tests ────────────────────────────────────────

    #[test]
    fn log_level_from_str_debug() {
        let val = LogLevel::from_str("DEBUG", true).unwrap();
        assert_eq!(val, LogLevel::Debug);
    }

    #[test]
    fn log_level_from_str_info() {
        let val = LogLevel::from_str("INFO", true).unwrap();
        assert_eq!(val, LogLevel::Info);
    }

    #[test]
    fn log_level_from_str_warning() {
        let val = LogLevel::from_str("WARNING", true).unwrap();
        assert_eq!(val, LogLevel::Warning);
    }

    #[test]
    fn log_level_from_str_error() {
        let val = LogLevel::from_str("ERROR", true).unwrap();
        assert_eq!(val, LogLevel::Error);
    }

    #[test]
    fn log_level_invalid_value() {
        let result = LogLevel::from_str("TRACE", true);
        assert!(result.is_err());
    }

    // ── LogLevel::to_level_filter tests ────────────────────────────

    #[test]
    fn log_level_to_level_filter_mappings() {
        assert_eq!(
            LogLevel::Debug.to_level_filter(),
            tracing::level_filters::LevelFilter::DEBUG
        );
        assert_eq!(
            LogLevel::Info.to_level_filter(),
            tracing::level_filters::LevelFilter::INFO
        );
        assert_eq!(
            LogLevel::Warning.to_level_filter(),
            tracing::level_filters::LevelFilter::WARN
        );
        assert_eq!(
            LogLevel::Error.to_level_filter(),
            tracing::level_filters::LevelFilter::ERROR
        );
    }

    // ── LogLevel::to_filter_str tests ──────────────────────────────

    #[test]
    fn log_level_to_filter_str_mappings() {
        assert_eq!(LogLevel::Debug.to_filter_str(), "debug");
        assert_eq!(LogLevel::Info.to_filter_str(), "info");
        assert_eq!(LogLevel::Warning.to_filter_str(), "warn");
        assert_eq!(LogLevel::Error.to_filter_str(), "error");
    }

    // ── init_tracing tests ─────────────────────────────────────────

    // Note: tracing subscriber can only be initialised once per process.
    // We test that the function compiles and the building logic is sound
    // by verifying the filter string mapping (tested above).
    // A full integration test would call init_tracing in an isolated process.

    // ── CliArgs parsing tests ──────────────────────────────────────

    fn parse_args(args: &[&str]) -> Result<CliArgs, clap::Error> {
        CliArgs::try_parse_from(args)
    }

    #[test]
    fn cli_args_minimal_defaults() {
        let args = parse_args(&["apcore-mcp", "--extensions-dir", "/tmp/ext"]).unwrap();
        assert_eq!(args.extensions_dir, PathBuf::from("/tmp/ext"));
        assert_eq!(args.transport, Transport::Stdio);
        assert_eq!(args.host, "127.0.0.1");
        assert_eq!(args.port, 8000);
        assert_eq!(args.name, "apcore-mcp");
        assert!(args.version.is_none());
        assert_eq!(args.log_level, LogLevel::Info);
        assert!(!args.explorer);
        assert_eq!(args.explorer_prefix, "/explorer");
        assert!(!args.allow_execute);
        assert_eq!(args.explorer_title, "MCP Tool Explorer");
        assert!(args.explorer_project_name.is_none());
        assert!(args.explorer_project_url.is_none());
        assert!(args.jwt_secret.is_none());
        assert!(args.jwt_key_file.is_none());
        assert_eq!(args.jwt_algorithm, "HS256");
        assert!(args.jwt_audience.is_none());
        assert!(args.jwt_issuer.is_none());
        assert!(args.jwt_require_auth);
        assert!(args.exempt_paths.is_none());
        assert_eq!(args.approval, ApprovalMode::Off);
        assert!(args.strategy.is_none());
    }

    #[test]
    fn cli_args_full() {
        let args = parse_args(&[
            "apcore-mcp",
            "--extensions-dir",
            "/opt/ext",
            "--transport",
            "streamable-http",
            "--host",
            "0.0.0.0",
            "--port",
            "9090",
            "--name",
            "my-server",
            "--version",
            "1.2.3",
            "--log-level",
            "WARNING",
            "--explorer",
            "--explorer-prefix",
            "/tools",
            "--allow-execute",
            "--explorer-title",
            "My Custom Explorer",
            "--explorer-project-name",
            "My Project",
            "--explorer-project-url",
            "https://example.com",
            "--jwt-secret",
            "s3cret",
            "--jwt-algorithm",
            "RS256",
            "--jwt-audience",
            "my-app",
            "--jwt-issuer",
            "issuer.example.com",
            "--jwt-key-file",
            "/tmp/key.pem",
            "--jwt-require-auth",
            "false",
            "--exempt-paths",
            "/health,/metrics",
            "--approval",
            "auto-approve",
        ])
        .unwrap();

        assert_eq!(args.extensions_dir, PathBuf::from("/opt/ext"));
        assert_eq!(args.transport, Transport::StreamableHttp);
        assert_eq!(args.host, "0.0.0.0");
        assert_eq!(args.port, 9090);
        assert_eq!(args.name, "my-server");
        assert_eq!(args.version.as_deref(), Some("1.2.3"));
        assert_eq!(args.log_level, LogLevel::Warning);
        assert!(args.explorer);
        assert_eq!(args.explorer_prefix, "/tools");
        assert!(args.allow_execute);
        assert_eq!(args.explorer_title, "My Custom Explorer");
        assert_eq!(args.explorer_project_name.as_deref(), Some("My Project"));
        assert_eq!(
            args.explorer_project_url.as_deref(),
            Some("https://example.com")
        );
        assert_eq!(args.jwt_secret.as_deref(), Some("s3cret"));
        assert_eq!(args.jwt_algorithm, "RS256");
        assert_eq!(args.jwt_audience.as_deref(), Some("my-app"));
        assert_eq!(args.jwt_issuer.as_deref(), Some("issuer.example.com"));
        assert_eq!(args.jwt_key_file, Some(PathBuf::from("/tmp/key.pem")));
        assert!(!args.jwt_require_auth);
        assert_eq!(args.exempt_paths.as_deref(), Some("/health,/metrics"));
        assert_eq!(args.approval, ApprovalMode::AutoApprove);
    }

    #[test]
    fn cli_args_missing_extensions_dir_errors() {
        let result = parse_args(&["apcore-mcp"]);
        assert!(result.is_err());
    }

    #[test]
    fn cli_args_transport_streamable_http() {
        let args = parse_args(&[
            "apcore-mcp",
            "--extensions-dir",
            "/tmp/ext",
            "--transport",
            "streamable-http",
        ])
        .unwrap();
        assert_eq!(args.transport, Transport::StreamableHttp);
    }

    #[test]
    fn cli_args_approval_auto_approve() {
        let args = parse_args(&[
            "apcore-mcp",
            "--extensions-dir",
            "/tmp/ext",
            "--approval",
            "auto-approve",
        ])
        .unwrap();
        assert_eq!(args.approval, ApprovalMode::AutoApprove);
    }

    #[test]
    fn cli_args_log_level_warning() {
        let args = parse_args(&[
            "apcore-mcp",
            "--extensions-dir",
            "/tmp/ext",
            "--log-level",
            "WARNING",
        ])
        .unwrap();
        assert_eq!(args.log_level, LogLevel::Warning);
    }

    #[test]
    fn cli_args_jwt_require_auth_defaults_true() {
        let args = parse_args(&["apcore-mcp", "--extensions-dir", "/tmp/ext"]).unwrap();
        assert!(args.jwt_require_auth);
    }

    #[test]
    fn cli_args_no_jwt_require_auth_sets_false() {
        let args = parse_args(&[
            "apcore-mcp",
            "--extensions-dir",
            "/tmp/ext",
            "--jwt-require-auth",
            "false",
        ])
        .unwrap();
        assert!(!args.jwt_require_auth);
    }

    #[test]
    fn cli_args_exempt_paths_stored_raw() {
        let args = parse_args(&[
            "apcore-mcp",
            "--extensions-dir",
            "/tmp/ext",
            "--exempt-paths",
            "/health,/metrics",
        ])
        .unwrap();
        assert_eq!(args.exempt_paths.as_deref(), Some("/health,/metrics"));
    }

    #[test]
    fn cli_args_port_zero_rejected() {
        let result = parse_args(&["apcore-mcp", "--extensions-dir", "/tmp/ext", "--port", "0"]);
        assert!(result.is_err());
    }

    #[test]
    fn cli_args_transport_sse() {
        let args = parse_args(&[
            "apcore-mcp",
            "--extensions-dir",
            "/tmp/ext",
            "--transport",
            "sse",
        ])
        .unwrap();
        assert_eq!(args.transport, Transport::Sse);
    }

    #[test]
    fn cli_args_port_max_accepted() {
        let args = parse_args(&[
            "apcore-mcp",
            "--extensions-dir",
            "/tmp/ext",
            "--port",
            "65535",
        ])
        .unwrap();
        assert_eq!(args.port, 65535);
    }

    // ── validate_args tests ───────────────────────────────────────────

    #[test]
    fn validate_args_nonexistent_extensions_dir() {
        let args = parse_args(&[
            "apcore-mcp",
            "--extensions-dir",
            "/nonexistent/path/does/not/exist",
        ])
        .unwrap();
        let err = validate_args(&args).unwrap_err();
        assert_eq!(err.exit_code(), 1);
        assert!(err.to_string().contains("does not exist"));
    }

    #[test]
    fn validate_args_file_as_extensions_dir() {
        let f = NamedTempFile::new().unwrap();
        let args =
            parse_args(&["apcore-mcp", "--extensions-dir", f.path().to_str().unwrap()]).unwrap();
        let err = validate_args(&args).unwrap_err();
        assert_eq!(err.exit_code(), 1);
        assert!(err.to_string().contains("not a directory"));
    }

    #[test]
    fn validate_args_name_too_long() {
        let long_name = "x".repeat(256);
        let dir = tempfile::tempdir().unwrap();
        let args = parse_args(&[
            "apcore-mcp",
            "--extensions-dir",
            dir.path().to_str().unwrap(),
            "--name",
            &long_name,
        ])
        .unwrap();
        let err = validate_args(&args).unwrap_err();
        assert_eq!(err.exit_code(), 1);
        assert!(err.to_string().contains("255"));
    }

    /// The `--name` bound counts CHARACTERS, not UTF-8 bytes.
    ///
    /// `validate_args` is the CLI's first gate, ahead of the builder, so a
    /// byte count here rejected a 100-character CJK name (300 bytes) that
    /// Python and TypeScript both accept.
    #[test]
    fn validate_args_accepts_multibyte_name_under_the_char_limit() {
        // Escape form keeps this source file ASCII-only (apdev check-chars).
        let name: String = "\u{670D}".repeat(100);
        assert_eq!(name.len(), 300, "fixture must be multi-byte");
        let dir = tempfile::tempdir().unwrap();
        let args = parse_args(&[
            "apcore-mcp",
            "--extensions-dir",
            dir.path().to_str().unwrap(),
            "--name",
            &name,
        ])
        .unwrap();
        validate_args(&args).expect("a 100-character name is within the limit");
    }

    #[test]
    fn validate_args_valid() {
        let dir = tempfile::tempdir().unwrap();
        let args = parse_args(&[
            "apcore-mcp",
            "--extensions-dir",
            dir.path().to_str().unwrap(),
        ])
        .unwrap();
        assert!(validate_args(&args).is_ok());
    }

    // ── resolve_jwt_key tests ─────────────────────────────────────────

    #[test]
    fn resolve_jwt_key_from_file() {
        let mut f = NamedTempFile::new().unwrap();
        writeln!(f, "  my-secret-key  ").unwrap();
        let args = parse_args(&[
            "apcore-mcp",
            "--extensions-dir",
            "/tmp",
            "--jwt-key-file",
            f.path().to_str().unwrap(),
        ])
        .unwrap();
        let key = resolve_jwt_key(&args).unwrap();
        assert_eq!(key.as_deref(), Some("my-secret-key"));
    }

    #[test]
    fn resolve_jwt_key_file_not_found() {
        let args = parse_args(&[
            "apcore-mcp",
            "--extensions-dir",
            "/tmp",
            "--jwt-key-file",
            "/nonexistent/key.pem",
        ])
        .unwrap();
        let err = resolve_jwt_key(&args).unwrap_err();
        assert_eq!(err.exit_code(), 1);
        assert!(err.to_string().contains("does not exist"));
    }

    #[test]
    fn resolve_jwt_key_from_secret_arg() {
        let args = parse_args(&[
            "apcore-mcp",
            "--extensions-dir",
            "/tmp",
            "--jwt-secret",
            "inline-secret",
        ])
        .unwrap();
        let key = resolve_jwt_key(&args).unwrap();
        assert_eq!(key.as_deref(), Some("inline-secret"));
    }

    /// Test both env-based key resolution and the "nothing set" path
    /// in a single test to avoid parallel env var races.
    #[test]
    fn resolve_jwt_key_env_and_none() {
        // First: with APCORE_JWT_SECRET set, resolve_jwt_key returns it.
        // Safety: test-only env var manipulation.
        unsafe {
            std::env::set_var("APCORE_JWT_SECRET", "env-secret");
        }
        let args = parse_args(&["apcore-mcp", "--extensions-dir", "/tmp"]).unwrap();
        let key = resolve_jwt_key(&args).unwrap();
        assert_eq!(key.as_deref(), Some("env-secret"));

        // Second: without APCORE_JWT_SECRET, resolve_jwt_key returns None.
        unsafe {
            std::env::remove_var("APCORE_JWT_SECRET");
        }
        let args2 = parse_args(&["apcore-mcp", "--extensions-dir", "/tmp"]).unwrap();
        let key2 = resolve_jwt_key(&args2).unwrap();
        assert!(key2.is_none());
    }

    #[test]
    fn resolve_jwt_key_file_overrides_secret() {
        let mut f = NamedTempFile::new().unwrap();
        write!(f, "file-key").unwrap();
        let args = parse_args(&[
            "apcore-mcp",
            "--extensions-dir",
            "/tmp",
            "--jwt-key-file",
            f.path().to_str().unwrap(),
            "--jwt-secret",
            "arg-secret",
        ])
        .unwrap();
        let key = resolve_jwt_key(&args).unwrap();
        assert_eq!(key.as_deref(), Some("file-key"));
    }

    // ── parse_exempt_paths tests ──────────────────────────────────────

    #[test]
    fn parse_exempt_paths_basic() {
        let set = parse_exempt_paths("/health,/metrics");
        assert_eq!(set.len(), 2);
        assert!(set.contains("/health"));
        assert!(set.contains("/metrics"));
    }

    #[test]
    fn parse_exempt_paths_trims_whitespace() {
        let set = parse_exempt_paths(" /a , /b ");
        assert_eq!(set.len(), 2);
        assert!(set.contains("/a"));
        assert!(set.contains("/b"));
    }

    #[test]
    fn parse_exempt_paths_empty_string() {
        let set = parse_exempt_paths("");
        assert!(set.is_empty());
    }

    // ── transport_to_str tests ────────────────────────────────────────

    #[test]
    fn transport_to_str_mappings() {
        assert_eq!(transport_to_str(&Transport::Stdio), "stdio");
        assert_eq!(
            transport_to_str(&Transport::StreamableHttp),
            "streamable-http"
        );
        assert_eq!(transport_to_str(&Transport::Sse), "sse");
    }

    // ── CliError tests ────────────────────────────────────────────────

    #[test]
    fn cli_error_exit_codes() {
        let e1 = CliError::InvalidArgs("bad".into());
        assert_eq!(e1.exit_code(), 1);

        let e2 = CliError::StartupFailure("fail".into());
        assert_eq!(e2.exit_code(), 2);
    }

    #[test]
    fn parse_jwt_algorithm_known_values() {
        use jsonwebtoken::Algorithm;
        assert_eq!(parse_jwt_algorithm("HS256"), Algorithm::HS256);
        assert_eq!(parse_jwt_algorithm("HS384"), Algorithm::HS384);
        assert_eq!(parse_jwt_algorithm("RS256"), Algorithm::RS256);
        assert_eq!(parse_jwt_algorithm("ES256"), Algorithm::ES256);
        assert_eq!(parse_jwt_algorithm("EdDSA"), Algorithm::EdDSA);
    }

    #[test]
    fn parse_jwt_algorithm_case_insensitive() {
        use jsonwebtoken::Algorithm;
        assert_eq!(parse_jwt_algorithm("hs256"), Algorithm::HS256);
        assert_eq!(parse_jwt_algorithm("rs256"), Algorithm::RS256);
    }

    // ── StrategyPreset enum tests ────────────────────────────────────

    #[test]
    fn strategy_from_str_standard() {
        let val = StrategyPreset::from_str("standard", true).unwrap();
        assert_eq!(val, StrategyPreset::Standard);
    }

    #[test]
    fn strategy_from_str_internal() {
        let val = StrategyPreset::from_str("internal", true).unwrap();
        assert_eq!(val, StrategyPreset::Internal);
    }

    #[test]
    fn strategy_from_str_testing() {
        let val = StrategyPreset::from_str("testing", true).unwrap();
        assert_eq!(val, StrategyPreset::Testing);
    }

    #[test]
    fn strategy_from_str_performance() {
        let val = StrategyPreset::from_str("performance", true).unwrap();
        assert_eq!(val, StrategyPreset::Performance);
    }

    #[test]
    fn strategy_from_str_minimal() {
        let val = StrategyPreset::from_str("minimal", true).unwrap();
        assert_eq!(val, StrategyPreset::Minimal);
    }

    #[test]
    fn strategy_invalid_value() {
        let result = StrategyPreset::from_str("unknown", true);
        assert!(result.is_err());
    }

    #[test]
    fn strategy_as_str_mappings() {
        assert_eq!(StrategyPreset::Standard.as_str(), "standard");
        assert_eq!(StrategyPreset::Internal.as_str(), "internal");
        assert_eq!(StrategyPreset::Testing.as_str(), "testing");
        assert_eq!(StrategyPreset::Performance.as_str(), "performance");
        assert_eq!(StrategyPreset::Minimal.as_str(), "minimal");
    }

    #[test]
    fn cli_args_strategy_flag() {
        let args = parse_args(&[
            "apcore-mcp",
            "--extensions-dir",
            "/tmp/ext",
            "--strategy",
            "testing",
        ])
        .unwrap();
        assert_eq!(args.strategy, Some(StrategyPreset::Testing));
    }

    #[test]
    fn cli_args_strategy_default_none() {
        let args = parse_args(&["apcore-mcp", "--extensions-dir", "/tmp/ext"]).unwrap();
        assert!(args.strategy.is_none());
    }

    #[test]
    fn parse_jwt_algorithm_unknown_falls_back_to_hs256() {
        use jsonwebtoken::Algorithm;
        assert_eq!(parse_jwt_algorithm("UNKNOWN"), Algorithm::HS256);
    }
}
