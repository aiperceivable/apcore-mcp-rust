//! Process stdout belongs exclusively to one real stdio transport. Keep a
//! private protocol descriptor while ordinary stdout follows stderr. Injected
//! streams and HTTP transports never enter this process-wide boundary.

use std::fs::File;
use std::future::Future;
use std::io::{self, Write};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

static OWNED: AtomicBool = AtomicBool::new(false);
tokio::task_local! { static CURRENT: Arc<Owner>; }

struct Owner {
    capture: Mutex<Option<platform::Capture>>,
    transport_running: AtomicBool,
    pending: Mutex<Vec<PendingExecution>>,
}

struct PendingExecution {
    token: apcore::cancel::CancelToken,
    task: tokio::task::JoinHandle<()>,
}

// apcore 0.32.2 predates execution draining. Inherent methods take precedence
// when compiled against the core fix; the legacy fallback preserves this
// crate's declared minimum dependency and cannot promise async teardown.
#[allow(dead_code)]
trait LegacyExecutionDrain {
    fn has_active_executions(&self) -> bool {
        false
    }
    async fn drain(&self) {}
}
impl LegacyExecutionDrain for apcore::cancel::CancelToken {}

impl Owner {
    fn acquire() -> io::Result<Arc<Self>> {
        OWNED
            .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
            .map_err(|_| {
                io::Error::new(
                    io::ErrorKind::AlreadyExists,
                    "a stdio server already owns process stdout",
                )
            })?;
        let capture = platform::Capture::acquire();
        match capture {
            Ok(capture) => Ok(Arc::new(Self {
                capture: Mutex::new(Some(capture)),
                transport_running: AtomicBool::new(false),
                pending: Mutex::new(Vec::new()),
            })),
            Err(error) => {
                OWNED.store(false, Ordering::Release);
                Err(error)
            }
        }
    }

    fn restore(&self) -> io::Result<()> {
        let mut capture = self
            .capture
            .lock()
            .unwrap_or_else(|poison| poison.into_inner());
        if let Some(active) = capture.as_ref() {
            // Flush pending ordinary prints while stdout still targets stderr.
            // A failed flush can retain diagnostic bytes: fail closed rather
            // than let a later owner flush them into the protocol endpoint.
            io::stdout().lock().flush()?;
            active.restore()?;
            capture.take();
            OWNED.store(false, Ordering::Release);
        }
        Ok(())
    }
}

struct ScopeExit(Arc<Owner>);
impl Drop for ScopeExit {
    fn drop(&mut self) {
        for execution in self
            .0
            .pending
            .lock()
            .unwrap_or_else(|poison| poison.into_inner())
            .iter()
        {
            execution.token.cancel();
        }
    }
}

impl Drop for Owner {
    fn drop(&mut self) {
        if let Err(error) = self.restore() {
            // Never panic during cancellation or unwinding. Keep ownership
            // reserved if restoration failed so another server cannot capture
            // an already redirected descriptor as its protocol output.
            let _ = writeln!(
                io::stderr().lock(),
                "failed to restore stdout after stdio server: {error}"
            );
            // On Windows the process standard handle still borrows the owned
            // stderr duplicate. Retain the capture on unrecoverable failure:
            // closing it would leave a dangling standard handle. This bounded
            // leak is preferable to closing an active process output stream.
            if let Some(capture) = self
                .capture
                .get_mut()
                .unwrap_or_else(|poison| poison.into_inner())
                .take()
            {
                std::mem::forget(capture);
            }
        }
    }
}

pub(crate) async fn scope<F: Future>(enabled: bool, future: F) -> io::Result<F::Output> {
    if !enabled || CURRENT.try_with(|_| ()).is_ok() {
        return Ok(future.await);
    }
    let owner = Owner::acquire()?;
    let _exit = ScopeExit(owner.clone());
    let result = CURRENT.scope(owner.clone(), future).await;
    drain_owner(&owner).await;
    owner.restore()?;
    Ok(result)
}

pub(crate) fn scope_sync<T>(enabled: bool, operation: impl FnOnce() -> T) -> io::Result<T> {
    if !enabled || CURRENT.try_with(|_| ()).is_ok() {
        return Ok(operation());
    }
    let owner = Owner::acquire()?;
    let _exit = ScopeExit(owner.clone());
    let result = CURRENT.sync_scope(owner.clone(), operation);
    owner.restore()?;
    Ok(result)
}

/// Keeps the transport claim until its writer and in-flight calls have drained.
pub(crate) struct TransportClaim(Arc<Owner>);

/// Retained by SDK-owned cleanup futures if the serving future is cancelled.
#[derive(Clone)]
pub(crate) struct CleanupLease {
    _owner: Arc<Owner>,
}

impl CleanupLease {
    pub(crate) fn retain_execution(&self, token: apcore::cancel::CancelToken) {
        if !token.has_active_executions() {
            return;
        }
        // The MCP request owns this root token. Once its handler has exited or
        // been dropped, signal late/cooperative module work before retaining
        // its cleanup; no caller-supplied parent token is cancelled here.
        token.cancel();
        let owner = self._owner.clone();
        let retained_token = token.clone();
        let task = tokio::spawn(async move {
            let _owner = owner;
            retained_token.drain().await;
        });
        let mut pending = self
            ._owner
            .pending
            .lock()
            .unwrap_or_else(|poison| poison.into_inner());
        pending.retain(|execution| !execution.task.is_finished());
        pending.push(PendingExecution { token, task });
    }
}

async fn drain_owner(owner: &Owner) {
    let pending = std::mem::take(
        &mut *owner
            .pending
            .lock()
            .unwrap_or_else(|poison| poison.into_inner()),
    );
    for execution in pending {
        execution.token.cancel();
        let _ = execution.task.await;
    }
}

pub(crate) async fn drain_registered() {
    if let Ok(owner) = CURRENT.try_with(Clone::clone) {
        drain_owner(&owner).await;
    }
}

pub(crate) fn cleanup_lease() -> Option<CleanupLease> {
    CURRENT
        .try_with(|owner| CleanupLease {
            _owner: owner.clone(),
        })
        .ok()
}

/// Retains stdout ownership through the actual destruction of SDK background work.
pub(crate) struct BackgroundTask {
    task: Option<tokio::task::JoinHandle<()>>,
    lease: Option<CleanupLease>,
}

impl BackgroundTask {
    pub(crate) fn new(task: tokio::task::JoinHandle<()>) -> Self {
        Self {
            task: Some(task),
            lease: cleanup_lease(),
        }
    }

    fn abort_and_watch(&mut self) -> Option<tokio::task::JoinHandle<()>> {
        let task = self.task.take()?;
        task.abort();
        let lease = self.lease.take();
        let completion = async move {
            let _lease = lease;
            let _ = task.await;
        };
        if let Ok(runtime) = tokio::runtime::Handle::try_current() {
            Some(runtime.spawn(completion))
        } else {
            // Hosts may drop a previously-polled future outside its runtime.
            // Awaiting an owned JoinHandle itself needs no Tokio context.
            std::thread::spawn(move || futures::executor::block_on(completion));
            None
        }
    }

    pub(crate) async fn stop(mut self) {
        if let Some(completion) = self.abort_and_watch() {
            let _ = completion.await;
        }
    }
}

impl Drop for BackgroundTask {
    fn drop(&mut self) {
        self.abort_and_watch();
    }
}

impl Drop for TransportClaim {
    fn drop(&mut self) {
        self.0.transport_running.store(false, Ordering::Release);
    }
}

pub(crate) fn protocol_writer() -> io::Result<(File, TransportClaim)> {
    let owner = CURRENT
        .try_with(Clone::clone)
        .map_err(|_| io::Error::other("stdio ownership scope is missing"))?;
    owner
        .transport_running
        .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
        .map_err(|_| {
            io::Error::new(
                io::ErrorKind::AlreadyExists,
                "a stdio transport is already running in this scope",
            )
        })?;
    let claim = TransportClaim(owner.clone());
    let writer = owner
        .capture
        .lock()
        .unwrap_or_else(|poison| poison.into_inner())
        .as_ref()
        .ok_or_else(|| io::Error::other("stdio ownership has ended"))?
        .protocol_writer()?;
    Ok((writer, claim))
}

/// The blocking operation owns both the entire submitted frame and its
/// transport claim. Dropping the async writer cannot detach physical stdout
/// activity from the scope that owns it, or truncate a frame between chunks.
pub(crate) struct ProtocolWriter {
    file: Arc<Mutex<File>>,
    claim: Arc<TransportClaim>,
    pending: Option<(bool, tokio::task::JoinHandle<io::Result<usize>>)>,
}

impl ProtocolWriter {
    pub(crate) fn new(file: File, claim: TransportClaim) -> Self {
        Self {
            file: Arc::new(Mutex::new(file)),
            claim: Arc::new(claim),
            pending: None,
        }
    }

    fn start(&mut self, bytes: Option<Vec<u8>>) {
        let file = self.file.clone();
        let claim = self.claim.clone();
        let flushing = bytes.is_none();
        self.pending = Some((
            flushing,
            tokio::task::spawn_blocking(move || {
                let _claim = claim;
                let mut file = file.lock().unwrap_or_else(|poison| poison.into_inner());
                match bytes {
                    Some(bytes) => {
                        file.write_all(&bytes)?;
                        Ok(bytes.len())
                    }
                    None => {
                        file.flush()?;
                        Ok(0)
                    }
                }
            }),
        ));
    }

    fn poll_pending(
        &mut self,
        context: &mut std::task::Context<'_>,
    ) -> std::task::Poll<io::Result<usize>> {
        use std::task::Poll;
        let (_, task) = self.pending.as_mut().expect("pending protocol operation");
        match std::pin::Pin::new(task).poll(context) {
            Poll::Pending => Poll::Pending,
            Poll::Ready(result) => {
                self.pending = None;
                Poll::Ready(result.map_err(io::Error::other).and_then(|result| result))
            }
        }
    }
}

impl tokio::io::AsyncWrite for ProtocolWriter {
    fn poll_write(
        mut self: std::pin::Pin<&mut Self>,
        context: &mut std::task::Context<'_>,
        bytes: &[u8],
    ) -> std::task::Poll<io::Result<usize>> {
        use std::task::Poll;
        loop {
            if let Some((flushing, _)) = self.pending.as_ref() {
                let flushing = *flushing;
                match self.poll_pending(context) {
                    Poll::Pending => return Poll::Pending,
                    Poll::Ready(Err(error)) => return Poll::Ready(Err(error)),
                    Poll::Ready(Ok(written)) if !flushing => return Poll::Ready(Ok(written)),
                    Poll::Ready(Ok(_)) => {}
                }
            }
            if bytes.is_empty() {
                return Poll::Ready(Ok(0));
            }
            self.start(Some(bytes.to_vec()));
        }
    }

    fn poll_flush(
        mut self: std::pin::Pin<&mut Self>,
        context: &mut std::task::Context<'_>,
    ) -> std::task::Poll<io::Result<()>> {
        use std::task::Poll;
        loop {
            if let Some((flushing, _)) = self.pending.as_ref() {
                let flushing = *flushing;
                match self.poll_pending(context) {
                    Poll::Pending => return Poll::Pending,
                    Poll::Ready(Err(error)) => return Poll::Ready(Err(error)),
                    Poll::Ready(Ok(_)) if flushing => return Poll::Ready(Ok(())),
                    Poll::Ready(Ok(_)) => {}
                }
            }
            self.start(None);
        }
    }

    fn poll_shutdown(
        self: std::pin::Pin<&mut Self>,
        context: &mut std::task::Context<'_>,
    ) -> std::task::Poll<io::Result<()>> {
        self.poll_flush(context)
    }
}

#[cfg(unix)]
mod platform {
    use super::*;

    pub(super) struct Capture {
        original: File,
    }

    impl Capture {
        pub(super) fn acquire() -> io::Result<Self> {
            // Both duplicates are made before mutating stdout. OwnedFd -> File
            // transfers only the duplicate, never ownership of fd 1 or fd 2.
            let original = File::from(rustix::io::fcntl_dupfd_cloexec(io::stdout(), 3)?);
            let stderr = rustix::io::fcntl_dupfd_cloexec(io::stderr(), 3)?;
            let mut stdout = io::stdout().lock();
            stdout.flush()?;
            rustix::stdio::dup2_stdout(stderr)?;
            Ok(Self { original })
        }
        pub(super) fn protocol_writer(&self) -> io::Result<File> {
            self.original.try_clone()
        }
        pub(super) fn restore(&self) -> io::Result<()> {
            rustix::stdio::dup2_stdout(&self.original).map_err(Into::into)
        }
    }
}

#[cfg(windows)]
mod platform {
    use super::*;
    use std::os::windows::io::{AsHandle, AsRawHandle};
    use windows_sys::Win32::Foundation::INVALID_HANDLE_VALUE;
    use windows_sys::Win32::System::Console::{GetStdHandle, SetStdHandle, STD_OUTPUT_HANDLE};

    #[cfg(test)]
    pub(super) static FAIL_RESTORE: AtomicBool = AtomicBool::new(false);

    pub(super) struct Capture {
        original: File,
        // GetStdHandle returns a borrowed process handle. Never close it.
        borrowed_original: isize,
        stderr: File,
    }

    impl Capture {
        pub(super) fn acquire() -> io::Result<Self> {
            // SAFETY: querying the process standard handle has no pointer arguments.
            let borrowed_original = unsafe { GetStdHandle(STD_OUTPUT_HANDLE) };
            if borrowed_original.is_null() || borrowed_original == INVALID_HANDLE_VALUE {
                return Err(io::Error::other(
                    "process stdout has no valid Windows standard handle",
                ));
            }
            let original = File::from(io::stdout().as_handle().try_clone_to_owned()?);
            let stderr = File::from(io::stderr().as_handle().try_clone_to_owned()?);
            io::stdout().lock().flush()?;
            // SAFETY: stderr owns a live duplicated handle and is retained until
            // after restoration. SetStdHandle borrows it, it does not own it.
            if unsafe { SetStdHandle(STD_OUTPUT_HANDLE, stderr.as_raw_handle()) } == 0 {
                return Err(io::Error::last_os_error());
            }
            Ok(Self {
                original,
                borrowed_original: borrowed_original as isize,
                stderr,
            })
        }
        pub(super) fn protocol_writer(&self) -> io::Result<File> {
            self.original.try_clone()
        }
        pub(super) fn restore(&self) -> io::Result<()> {
            #[cfg(test)]
            if FAIL_RESTORE.load(Ordering::Acquire) {
                return Err(io::Error::other(
                    "injected standard handle restoration failure",
                ));
            }
            // Keep the destination alive until the borrowed original is restored.
            let _destination = &self.stderr;
            // SAFETY: the borrowed process handle was validated at acquisition
            // and has never been closed or transferred to an owning wrapper.
            if unsafe { SetStdHandle(STD_OUTPUT_HANDLE, self.borrowed_original as _) } == 0 {
                return Err(io::Error::last_os_error());
            }
            Ok(())
        }
    }
}

#[cfg(not(any(unix, windows)))]
compile_error!("stdio stdout isolation requires Unix or Windows standard handles");

#[cfg(test)]
mod tests {
    use super::*;
    use std::process::Command;

    #[test]
    fn cancelled_backpressured_transport_keeps_frame_and_owner_until_write_finishes() {
        use std::io::{BufRead, Read};
        use std::process::Stdio;
        use std::time::Duration;
        const CHILD: &str = "APCORE_BACKPRESSURE_TEST";
        if std::env::var_os(CHILD).is_some() {
            struct LargeResponse(Arc<AtomicBool>);
            #[async_trait::async_trait]
            impl crate::server::transport::McpHandler for LargeResponse {
                async fn handle_message(
                    &self,
                    message: serde_json::Value,
                ) -> Option<serde_json::Value> {
                    let response = serde_json::json!({"jsonrpc":"2.0", "id":message["id"], "result":"x".repeat(2 * 1024 * 1024)});
                    self.0.store(true, Ordering::Release);
                    Some(response)
                }
            }
            tokio::runtime::Runtime::new().unwrap().block_on(async {
                let manager = crate::server::transport::TransportManager::new(None);
                let response_ready = Arc::new(AtomicBool::new(false));
                let handler = LargeResponse(response_ready.clone());
                let mut serving = Box::pin(manager.run_stdio(&handler));
                tokio::time::timeout(Duration::from_secs(3), async {
                    loop {
                        assert!(futures::poll!(serving.as_mut()).is_pending());
                        if response_ready.load(Ordering::Acquire) {
                            break;
                        }
                        tokio::task::yield_now().await;
                    }
                })
                .await
                .expect("response never reached its backed-up write");
                drop(serving);
                let rejected = scope(true, async {}).await.is_err();
                // Parent deliberately does not drain stdout until this marker.
                eprintln!("BACKPRESSURED_CANCELLED");
                assert!(
                    rejected,
                    "cancelled transport released an active physical write"
                );
                tokio::time::timeout(Duration::from_secs(3), async {
                    while OWNED.load(Ordering::Acquire) {
                        tokio::task::yield_now().await;
                    }
                })
                .await
                .expect("completed physical write retained ownership");
                scope(true, async {
                    let (mut writer, _claim) = protocol_writer().unwrap();
                    writeln!(writer, "{{\"jsonrpc\":\"2.0\",\"id\":2,\"result\":{{}}}}").unwrap();
                })
                .await
                .unwrap();
            });
            return;
        }
        let mut child = Command::new(std::env::current_exe().unwrap())
            .args(["--exact", "stdio::tests::cancelled_backpressured_transport_keeps_frame_and_owner_until_write_finishes", "--nocapture"])
            .env(CHILD, "1").stdin(Stdio::piped()).stdout(Stdio::piped()).stderr(Stdio::piped()).spawn().unwrap();
        child
            .stdin
            .take()
            .unwrap()
            .write_all(b"{\"jsonrpc\":\"2.0\",\"id\":1,\"method\":\"tools/call\"}\n")
            .unwrap();
        let stderr = child.stderr.take().unwrap();
        let (marker_tx, marker_rx) = std::sync::mpsc::channel();
        let stderr_thread = std::thread::spawn(move || {
            let mut reader = io::BufReader::new(stderr);
            let mut output = String::new();
            loop {
                let mut line = String::new();
                if reader.read_line(&mut line).unwrap() == 0 {
                    break;
                }
                output.push_str(&line);
                if line.contains("BACKPRESSURED_CANCELLED") {
                    let _ = marker_tx.send(());
                    reader.read_to_string(&mut output).unwrap();
                    break;
                }
            }
            output
        });
        if marker_rx.recv_timeout(Duration::from_secs(5)).is_err() {
            let _ = child.kill();
            panic!(
                "child failed to reach backed-up write cancellation: {}",
                stderr_thread.join().unwrap()
            );
        }
        let mut stdout = String::new();
        child
            .stdout
            .take()
            .unwrap()
            .read_to_string(&mut stdout)
            .unwrap();
        let status = child.wait().unwrap();
        let stderr = stderr_thread.join().unwrap();
        assert!(status.success(), "child failed: {stderr}");
        let frames: Vec<serde_json::Value> = stdout
            .lines()
            .filter(|line| line.starts_with('{'))
            .map(|line| {
                serde_json::from_str(line).expect("partial/interleaved frame after cancellation")
            })
            .collect();
        assert_eq!(frames.len(), 2);
        assert_eq!(frames[0]["id"], 1);
        assert_eq!(frames[0]["result"].as_str().unwrap().len(), 2 * 1024 * 1024);
        assert_eq!(frames[1]["id"], 2);
    }

    #[cfg(unix)]
    #[test]
    fn failed_diagnostic_flush_poison_prevents_protocol_reuse() {
        const CHILD: &str = "APCORE_FLUSH_FAILURE_TEST";
        if std::env::var_os(CHILD).is_some() {
            use std::os::unix::net::UnixStream;
            let original_stdout =
                File::from(rustix::io::fcntl_dupfd_cloexec(io::stdout(), 3).unwrap());
            let original_stderr =
                File::from(rustix::io::fcntl_dupfd_cloexec(io::stderr(), 3).unwrap());
            let (diagnostic, receiver) = UnixStream::pair().unwrap();
            drop(receiver);
            rustix::stdio::dup2_stderr(&diagnostic).unwrap();
            let failed = scope_sync(true, || {
                io::stdout().write_all(b"BUFFERED-DIAGNOSTIC").unwrap();
            })
            .is_err();
            let rejected = scope_sync(true, || ()).is_err();
            // Repair only the subprocess test's descriptors. Flush retained
            // bytes to stderr before allowing libtest to print its report.
            rustix::stdio::dup2_stderr(&original_stderr).unwrap();
            rustix::stdio::dup2_stdout(&original_stderr).unwrap();
            io::stdout().flush().unwrap();
            rustix::stdio::dup2_stdout(&original_stdout).unwrap();
            assert!(failed);
            assert!(rejected, "failed flush admitted another protocol owner");
            return;
        }
        let output = Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "stdio::tests::failed_diagnostic_flush_poison_prevents_protocol_reuse",
                "--nocapture",
            ])
            .env(CHILD, "1")
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        assert!(!String::from_utf8_lossy(&output.stdout).contains("BUFFERED-DIAGNOSTIC"));
        assert!(String::from_utf8_lossy(&output.stderr).contains("BUFFERED-DIAGNOSTIC"));
    }

    #[test]
    fn real_serving_entrypoint_isolates_noisy_module_and_lifecycle_callbacks() {
        use std::process::Stdio;
        const CHILD: &str = "APCORE_NOISY_LIFECYCLE_TEST";
        if std::env::var_os(CHILD).is_some() {
            struct NoisyModule;
            #[async_trait::async_trait]
            impl apcore::Module for NoisyModule {
                fn input_schema(&self) -> serde_json::Value {
                    serde_json::json!({"type":"object"})
                }
                fn output_schema(&self) -> serde_json::Value {
                    serde_json::json!({"type":"object"})
                }
                fn description(&self) -> &str {
                    "A successful noisy module"
                }
                async fn execute(
                    &self,
                    _: serde_json::Value,
                    _: &apcore::Context<serde_json::Value>,
                ) -> Result<serde_json::Value, apcore::ModuleError> {
                    struct Cleanup;
                    impl Drop for Cleanup {
                        fn drop(&mut self) {
                            writeln!(io::stdout(), "module-cleanup-diagnostic").unwrap();
                        }
                    }
                    let _cleanup = Cleanup;
                    writeln!(io::stdout(), "module-execution-diagnostic").unwrap();
                    Ok(serde_json::json!({"ok":true}))
                }
            }
            let registry = Arc::new(apcore::registry::registry::Registry::new());
            registry
                .register_module("noisy", Box::new(NoisyModule))
                .unwrap();
            // Existing subscribers may have retained stdout before serving.
            tracing_subscriber::fmt()
                .without_time()
                .with_ansi(false)
                .with_writer(io::stdout)
                .init();
            let mcp = crate::APCoreMCP::builder()
                .backend(registry)
                .build()
                .unwrap();
            tokio::runtime::Runtime::new()
                .unwrap()
                .block_on(mcp.serve_with_options_async(crate::ServeOptions {
                    on_startup: Some(Box::new(|| {
                        writeln!(io::stdout(), "startup-callback-diagnostic").unwrap()
                    })),
                    on_shutdown: Some(Box::new(|| {
                        writeln!(io::stdout(), "shutdown-callback-diagnostic").unwrap()
                    })),
                    ..Default::default()
                }))
                .unwrap();
            return;
        }
        let mut child = Command::new(std::env::current_exe().unwrap())
            .args(["--exact", "stdio::tests::real_serving_entrypoint_isolates_noisy_module_and_lifecycle_callbacks", "--nocapture"])
            .env(CHILD, "1").stdin(Stdio::piped()).stdout(Stdio::piped()).stderr(Stdio::piped()).spawn().unwrap();
        child.stdin.take().unwrap().write_all(concat!(
            "{\"jsonrpc\":\"2.0\",\"id\":1,\"method\":\"initialize\",\"params\":{\"protocolVersion\":\"2025-06-18\",\"capabilities\":{},\"clientInfo\":{\"name\":\"test\",\"version\":\"1\"}}}\n",
            "{\"jsonrpc\":\"2.0\",\"id\":2,\"method\":\"tools/list\"}\n",
            "{\"jsonrpc\":\"2.0\",\"id\":3,\"method\":\"tools/call\",\"params\":{\"name\":\"noisy\",\"arguments\":{}}}\n",
        ).as_bytes()).unwrap();
        let output = child.wait_with_output().unwrap();
        let stdout = String::from_utf8(output.stdout).unwrap();
        let stderr = String::from_utf8(output.stderr).unwrap();
        assert!(output.status.success(), "stdout={stdout}; stderr={stderr}");
        let frames: Vec<serde_json::Value> = stdout
            .lines()
            .filter(|line| line.starts_with('{'))
            .map(|line| serde_json::from_str(line).unwrap())
            .collect();
        assert_eq!(frames.len(), 3, "{stdout}");
        let call = frames.iter().find(|frame| frame["id"] == 3).unwrap();
        assert_ne!(call["result"]["isError"], true, "{call}");
        assert!(call["result"].to_string().contains("ok"), "{call}");
        for marker in [
            "Starting MCP server",
            "startup-callback-diagnostic",
            "module-execution-diagnostic",
            "module-cleanup-diagnostic",
            "shutdown-callback-diagnostic",
        ] {
            assert!(stderr.contains(marker), "missing {marker}: {stderr}");
            assert!(
                !stdout.contains(marker),
                "stdout diagnostic {marker}: {stdout}"
            );
        }
    }

    #[test]
    fn http_serving_entrypoints_keep_host_stdout_and_leave_stdio_available() {
        const CHILD: &str = "APCORE_HTTP_OUTPUT_TEST";
        if let Ok(transport) = std::env::var(CHILD) {
            let registry = Arc::new(apcore::registry::registry::Registry::new());
            let mcp = crate::APCoreMCP::builder()
                .backend(registry)
                .transport(&transport)
                // Exercise actual transport setup without requiring a listener.
                .host("invalid-host")
                .build()
                .unwrap();
            let result =
                tokio::runtime::Runtime::new()
                    .unwrap()
                    .block_on(mcp.serve_with_options_async(crate::ServeOptions {
                        on_startup: Some(Box::new(|| {
                            writeln!(io::stdout(), "http-startup-ordinary").unwrap();
                            scope_sync(true, || {
                                writeln!(io::stdout(), "temporary-stdio-diagnostic").unwrap()
                            })
                            .unwrap();
                        })),
                        on_shutdown: Some(Box::new(|| {
                            writeln!(io::stdout(), "http-shutdown-ordinary").unwrap()
                        })),
                        ..Default::default()
                    }));
            assert!(result.unwrap_err().to_string().contains("invalid host"));
            assert!(!OWNED.load(Ordering::Acquire));
            return;
        }
        for transport in ["streamable-http", "sse"] {
            let output = Command::new(std::env::current_exe().unwrap())
                .args(["--exact", "stdio::tests::http_serving_entrypoints_keep_host_stdout_and_leave_stdio_available", "--nocapture"])
                .env(CHILD, transport).output().unwrap();
            let stdout = String::from_utf8(output.stdout).unwrap();
            let stderr = String::from_utf8(output.stderr).unwrap();
            assert!(output.status.success(), "{transport}: {stderr}");
            assert!(
                stdout.contains("http-startup-ordinary"),
                "{transport}: {stdout}"
            );
            assert!(
                stdout.contains("http-shutdown-ordinary"),
                "{transport}: {stdout}"
            );
            assert!(!stdout.contains("temporary-stdio-diagnostic"));
            assert!(stderr.contains("temporary-stdio-diagnostic"));
        }
    }

    #[test]
    fn live_http_transports_keep_runtime_output_and_independent_stdio_ownership() {
        const CHILD: &str = "APCORE_LIVE_HTTP_OUTPUT_TEST";
        if let Ok(transport) = std::env::var(CHILD) {
            struct Handler;
            #[async_trait::async_trait]
            impl crate::server::transport::McpHandler for Handler {
                async fn handle_message(&self, _: serde_json::Value) -> Option<serde_json::Value> {
                    None
                }
            }
            tokio::runtime::Runtime::new().unwrap().block_on(async {
                use tokio::io::{AsyncReadExt, AsyncWriteExt};
                tokio::time::timeout(std::time::Duration::from_secs(5), async {
                    let port = std::net::TcpListener::bind("127.0.0.1:0").unwrap().local_addr().unwrap().port();
                    let extra = axum::Router::new().route("/host-output", axum::routing::get(|| async {
                        writeln!(io::stdout(), "live-http-runtime-ordinary").unwrap();
                        "live-response"
                    }));
                    let manager = Arc::new(crate::TransportManager::new(None));
                    let server = tokio::spawn(async move {
                        if transport == "streamable-http" {
                            manager.run_streamable_http(Arc::new(Handler), "127.0.0.1", port, Some(extra)).await
                        } else {
                            #[allow(deprecated)]
                            manager.run_sse(Arc::new(Handler), "127.0.0.1", port, Some(extra)).await
                        }
                    });
                    let mut client = loop {
                        assert!(!server.is_finished(), "HTTP server exited before listening");
                        if let Ok(client) = tokio::net::TcpStream::connect(("127.0.0.1", port)).await { break client; }
                        tokio::task::yield_now().await;
                    };
                    // A separate host task has no server task-local CURRENT.
                    // The listener must leave process stdout unclaimed while live.
                    tokio::spawn(async {
                        scope(true, async {
                            writeln!(io::stdout(), "independent-stdio-diagnostic").unwrap();
                        }).await.unwrap();
                    }).await.unwrap();
                    assert!(!server.is_finished());
                    client.write_all(b"GET /host-output HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n").await.unwrap();
                    let mut response = String::new();
                    client.read_to_string(&mut response).await.unwrap();
                    assert!(response.starts_with("HTTP/1.1 200"), "{response}");
                    assert!(response.ends_with("live-response"), "{response}");
                    server.abort();
                    assert!(server.await.unwrap_err().is_cancelled());
                    assert!(!OWNED.load(Ordering::Acquire));
                }).await.expect("live HTTP transport test timed out");
            });
            return;
        }
        for transport in ["streamable-http", "sse"] {
            let output = Command::new(std::env::current_exe().unwrap())
                .args(["--exact", "stdio::tests::live_http_transports_keep_runtime_output_and_independent_stdio_ownership", "--nocapture"])
                .env(CHILD, transport).output().unwrap();
            let stdout = String::from_utf8(output.stdout).unwrap();
            let stderr = String::from_utf8(output.stderr).unwrap();
            assert!(
                output.status.success(),
                "{transport}: stdout={stdout}; stderr={stderr}"
            );
            assert!(
                stdout.contains("live-http-runtime-ordinary"),
                "{transport}: {stdout}"
            );
            assert!(
                !stderr.contains("live-http-runtime-ordinary"),
                "{transport}: {stderr}"
            );
            assert!(
                !stdout.contains("independent-stdio-diagnostic"),
                "{transport}: {stdout}"
            );
            assert!(
                stderr.contains("independent-stdio-diagnostic"),
                "{transport}: {stderr}"
            );
        }
    }

    /// Descriptor mutation belongs in a subprocess; the Rust test harness runs
    /// unrelated tests concurrently and owns its own stdout capture machinery.
    #[test]
    fn subprocess_scope_lifecycle() {
        const CHILD_MODE: &str = "APCORE_STDIO_SCOPE_TEST";
        if let Ok(mode) = std::env::var(CHILD_MODE) {
            // Bypass libtest's thread-local print capture to exercise real OS IO.
            let ordinary = || writeln!(io::stdout(), "stdio-ordinary").unwrap();
            match mode.as_str() {
                "success" | "error" => {
                    let result = scope_sync(true, || {
                        ordinary();
                        scope_sync(true, || {
                            let (mut writer, _claim) = protocol_writer().unwrap();
                            writeln!(writer, "stdio-protocol").unwrap();
                            assert!(protocol_writer().is_err());
                        })
                        .unwrap();
                        if mode == "error" {
                            Err("expected server error")
                        } else {
                            Ok(())
                        }
                    })
                    .unwrap();
                    assert_eq!(result.is_err(), mode == "error");
                }
                "panic" => {
                    assert!(std::panic::catch_unwind(|| scope_sync(true, || {
                        ordinary();
                        panic!("expected startup callback panic");
                    }))
                    .is_err());
                }
                "cancel" | "concurrent" => {
                    tokio::runtime::Runtime::new().unwrap().block_on(async {
                        if mode == "cancel" {
                            let timed = tokio::time::timeout(
                                std::time::Duration::from_millis(20),
                                scope(true, async {
                                    ordinary();
                                    std::future::pending::<()>().await;
                                }),
                            )
                            .await;
                            assert!(timed.is_err());
                        } else {
                            scope(true, async {
                                ordinary();
                                let rejected = tokio::spawn(scope(true, async {})).await.unwrap();
                                assert_eq!(
                                    rejected.unwrap_err().kind(),
                                    io::ErrorKind::AlreadyExists
                                );
                            })
                            .await
                            .unwrap();
                        }
                    });
                }
                "disabled" => {
                    scope_sync(false, ordinary).unwrap();
                }
                #[cfg(windows)]
                "acquire-failure" => {
                    use windows_sys::Win32::System::Console::{
                        GetStdHandle, SetStdHandle, STD_OUTPUT_HANDLE,
                    };
                    // SAFETY: only this subprocess owns its standard handles.
                    // Preserve the borrowed original and restore before asserting.
                    let original = unsafe { GetStdHandle(STD_OUTPUT_HANDLE) };
                    assert_ne!(
                        unsafe { SetStdHandle(STD_OUTPUT_HANDLE, std::ptr::null_mut()) },
                        0
                    );
                    let failed = Owner::acquire().is_err();
                    let released = !OWNED.load(Ordering::Acquire);
                    assert_ne!(unsafe { SetStdHandle(STD_OUTPUT_HANDLE, original) }, 0);
                    assert!(failed && released);
                }
                #[cfg(windows)]
                "restore-failure" => {
                    use windows_sys::Win32::System::Console::{
                        GetStdHandle, SetStdHandle, STD_OUTPUT_HANDLE,
                    };
                    // SAFETY: subprocess-only test; retain the borrowed handle
                    // so the injected failed restoration can be repaired below.
                    let original = unsafe { GetStdHandle(STD_OUTPUT_HANDLE) };
                    let owner = Owner::acquire().unwrap();
                    platform::FAIL_RESTORE.store(true, Ordering::Release);
                    drop(owner);
                    // This fails with ERROR_INVALID_HANDLE if Owner::drop
                    // closes the duplicate still installed by SetStdHandle.
                    ordinary();
                    assert!(OWNED.load(Ordering::Acquire));
                    platform::FAIL_RESTORE.store(false, Ordering::Release);
                    // SAFETY: restore the original borrowed live process handle.
                    assert_ne!(unsafe { SetStdHandle(STD_OUTPUT_HANDLE, original) }, 0);
                    OWNED.store(false, Ordering::Release);
                }
                _ => panic!("unexpected child mode"),
            }
            writeln!(io::stdout(), "stdio-restored").unwrap();
            // A second owner proves release of process ownership on every path.
            scope_sync(true, || ()).unwrap();
            return;
        }
        let modes = [
            "success",
            "error",
            "panic",
            "cancel",
            "concurrent",
            "disabled",
        ];
        #[cfg(windows)]
        let modes = modes
            .into_iter()
            .chain(["acquire-failure", "restore-failure"]);
        for mode in modes {
            let output = Command::new(std::env::current_exe().unwrap())
                .args([
                    "--exact",
                    "stdio::tests::subprocess_scope_lifecycle",
                    "--nocapture",
                ])
                .env(CHILD_MODE, mode)
                .output()
                .unwrap();
            let stdout = String::from_utf8(output.stdout).unwrap();
            let stderr = String::from_utf8(output.stderr).unwrap();
            assert!(
                output.status.success(),
                "{mode}: stdout={stdout}, stderr={stderr}"
            );
            assert!(stdout.contains("stdio-restored"), "{mode}: {stdout}");
            assert_eq!(
                stdout.contains("stdio-ordinary"),
                mode == "disabled",
                "{mode}: {stdout}"
            );
            if !["disabled", "acquire-failure"].contains(&mode) {
                assert!(stderr.contains("stdio-ordinary"), "{mode}: {stderr}");
            }
            if ["success", "error"].contains(&mode) {
                assert!(stdout.contains("stdio-protocol"), "{mode}: {stdout}");
            }
        }
    }
}
