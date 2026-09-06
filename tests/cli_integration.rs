//! Integration tests for the apcore-mcp CLI binary.
//!
//! These tests invoke the compiled binary via `std::process::Command` and
//! verify exit codes and output for various argument combinations.

use std::process::Command;

/// Path to the compiled binary (built by `cargo build`).
fn binary_path() -> std::path::PathBuf {
    // cargo test sets the target directory; the binary lives alongside test binaries.
    let mut path = std::env::current_exe()
        .expect("current_exe")
        .parent()
        .expect("parent of test binary")
        .parent()
        .expect("parent of deps dir")
        .to_path_buf();
    path.push("apcore-mcp");
    path
}

#[test]
fn help_exits_zero() {
    let output = Command::new(binary_path())
        .arg("--help")
        .output()
        .expect("failed to run binary");
    assert!(
        output.status.success(),
        "expected exit 0, got {:?}",
        output.status.code()
    );
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        stdout.contains("extensions-dir"),
        "help should mention --extensions-dir"
    );
    assert!(
        stdout.contains("apcore-mcp"),
        "help should mention apcore-mcp"
    );
}

#[test]
fn missing_backend_source_exits_nonzero() {
    // Reversal: --extensions-dir is no longer clap-required (--from-openapi
    // and mcp.openapi on the Config Bus both count now — PRD F-054
    // Acceptance Criterion 1), so clap itself accepts zero args. The
    // backend-source rule moved into validate_args, which reports it as an
    // InvalidArgs error (exit 1), not a clap parse error (exit 2).
    let output = Command::new(binary_path())
        .output()
        .expect("failed to run binary");
    assert!(
        !output.status.success(),
        "expected non-zero exit, got {:?}",
        output.status.code()
    );
    assert_eq!(output.status.code(), Some(1));
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("a backend source is required"),
        "expected the backend-source message, got: {stderr}"
    );
}

#[test]
fn nonexistent_extensions_dir_exits_one() {
    let output = Command::new(binary_path())
        .args(["--extensions-dir", "/nonexistent/path/does/not/exist"])
        .output()
        .expect("failed to run binary");
    assert_eq!(
        output.status.code(),
        Some(1),
        "expected exit 1 for nonexistent extensions dir"
    );
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("does not exist"),
        "stderr should mention missing dir: {stderr}"
    );
}

#[test]
fn port_zero_exits_nonzero() {
    let output = Command::new(binary_path())
        .args(["--extensions-dir", "/tmp", "--port", "0"])
        .output()
        .expect("failed to run binary");
    assert!(
        !output.status.success(),
        "expected non-zero exit for port 0"
    );
    // clap validation exits with 2
    assert_eq!(output.status.code(), Some(2));
}

#[test]
fn name_too_long_exits_one() {
    let long_name = "x".repeat(256);
    let dir = tempfile::tempdir().unwrap();
    let output = Command::new(binary_path())
        .args([
            "--extensions-dir",
            dir.path().to_str().unwrap(),
            "--name",
            &long_name,
        ])
        .output()
        .expect("failed to run binary");
    assert_eq!(
        output.status.code(),
        Some(1),
        "expected exit 1 for name too long"
    );
}

#[test]
fn jwt_key_file_nonexistent_exits_one() {
    let dir = tempfile::tempdir().unwrap();
    let output = Command::new(binary_path())
        .args([
            "--extensions-dir",
            dir.path().to_str().unwrap(),
            "--jwt-key-file",
            "/nonexistent/key.pem",
        ])
        .output()
        .expect("failed to run binary");
    assert_eq!(
        output.status.code(),
        Some(1),
        "expected exit 1 for nonexistent jwt key file"
    );
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("does not exist"),
        "stderr should mention missing key file: {stderr}"
    );
}

/// [B-RS-1] The shipped binary must actually start and answer requests.
///
/// The earlier tests only covered `--help` and error paths, which let a
/// nested-runtime panic ship: `#[tokio::main]` in `bin/apcore-mcp.rs` plus a
/// `Runtime::new().block_on(..)` inside `serve()` aborted every transport at
/// startup. This drives a real stdio session end to end.
#[test]
fn stdio_transport_answers_tools_list() {
    use std::io::{BufRead, BufReader, Write};
    use std::process::Stdio;

    let dir = tempfile::tempdir().unwrap();
    let mut child = Command::new(binary_path())
        .args([
            "--extensions-dir",
            dir.path().to_str().unwrap(),
            "--transport",
            "stdio",
        ])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("failed to spawn binary");

    {
        let stdin = child.stdin.as_mut().expect("stdin");
        writeln!(
            stdin,
            r#"{{"jsonrpc":"2.0","id":1,"method":"initialize","params":{{"protocolVersion":"2024-11-05","capabilities":{{}},"clientInfo":{{"name":"conformance","version":"1"}}}}}}"#
        )
        .expect("write initialize");
        writeln!(stdin, r#"{{"jsonrpc":"2.0","id":2,"method":"tools/list"}}"#)
            .expect("write tools/list");
        stdin.flush().expect("flush");
    }

    // Read until the `tools/list` reply (id 2) arrives, then shut the child down.
    let stdout = child.stdout.take().expect("stdout");
    let mut tools_reply = None;
    for line in BufReader::new(stdout).lines() {
        let line = line.expect("read stdout line");
        let value: serde_json::Value = match serde_json::from_str(&line) {
            Ok(v) => v,
            Err(_) => continue,
        };
        if value.get("id").and_then(|v| v.as_u64()) == Some(2) {
            tools_reply = Some(value);
            break;
        }
    }

    let _ = child.kill();
    let _ = child.wait();

    let reply = tools_reply
        .expect("no tools/list response on stdio — the binary never reached a serving state");
    assert!(
        reply.get("error").is_none(),
        "tools/list returned an error: {reply}"
    );
    assert!(
        reply
            .pointer("/result/tools")
            .and_then(|t| t.as_array())
            .is_some(),
        "tools/list response has no result.tools array: {reply}"
    );
}
