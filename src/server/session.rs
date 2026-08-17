//! Live MCP sessions — the server-to-client half of the JSON-RPC channel.
//!
//! # Why this exists
//!
//! MCP is bidirectional: `elicitation/create` is a request the *server* sends
//! to the *client*, and the client answers it with an ordinary JSON-RPC
//! response. Every transport in this crate was client-to-server only. The
//! router already knew how to turn a [`SessionHandle`] into an
//! [`ElicitCallback`] (see [`make_elicit_callback`]), and
//! [`elicit_registry`] already knew how to hand that callback to an
//! `ApprovalHandler` — but no transport ever produced a `SessionHandle`, so
//! `ExecutionRouter::extract_extra` returned `None` for it unconditionally and
//! `apcore-mcp serve` could not prompt (apexe#29).
//!
//! This module supplies the missing half:
//!
//! * [`OutboundSink`] — how one connection writes a server-initiated message.
//!   Each transport implements it over whatever it already owns (an SSE event
//!   channel, the stdio writer).
//! * [`McpSession`] — one connection's request/response correlation state. It
//!   implements [`SessionHandle`], so the router's existing wiring picks it up
//!   with no further change.
//! * [`SessionRegistry`] — the map from the transport-minted session id (the
//!   one [`stamp_session_id`] writes onto every inbound message) back to the
//!   session, so a tool call dispatched from a plain JSON `_meta` can still
//!   reach the connection it arrived on.
//!
//! # Correlation
//!
//! JSON-RPC correlates by `id`, and the two directions have independent id
//! spaces. Ids minted here are uuid-based and prefixed, so a server request id
//! can never be confused with a client one while reading a transcript.
//!
//! A message is treated as an answer to a server request only when it has an
//! `id`, has no `method`, and that id is in this session's pending table.
//! Anything else is passed through to the handler unchanged — an unrecognised
//! id must still produce the handler's own error rather than being silently
//! swallowed here.
//!
//! [`ElicitCallback`]: crate::helpers::ElicitCallback
//! [`make_elicit_callback`]: crate::server::router
//! [`elicit_registry`]: crate::server::elicit_registry
//! [`stamp_session_id`]: crate::server::transport

use std::collections::HashMap;
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::Duration;

use async_trait::async_trait;
use serde_json::Value;
use tokio::sync::oneshot;

use crate::helpers::ElicitResult;
use crate::server::router::SessionHandle;

/// How long a server-initiated request waits for the client's answer.
///
/// Elicitation is a human-in-the-loop prompt, so the bound is generous: the
/// user has to read the request and decide. It exists at all because a client
/// that advertises the capability and then never answers must not pin the
/// tool call — and the pending entry — for the life of the connection.
pub const DEFAULT_ELICIT_TIMEOUT: Duration = Duration::from_secs(300);

/// Failure modes of a server-initiated request.
#[derive(Debug, thiserror::Error)]
pub enum SessionError {
    /// The client did not advertise `elicitation` during `initialize`.
    ///
    /// Sending the request anyway would leave the call waiting on an answer
    /// no conforming client is obliged to give, so it is refused up front.
    #[error("client did not advertise the elicitation capability")]
    ElicitationUnsupported,

    /// The transport could not write the request to the client.
    #[error("failed to send request to client: {0}")]
    Send(String),

    /// The client did not answer within [`DEFAULT_ELICIT_TIMEOUT`].
    #[error("client did not answer within {0:?}")]
    Timeout(Duration),

    /// The connection went away while the request was outstanding.
    #[error("session closed while awaiting a response")]
    Closed,

    /// The client answered with a JSON-RPC error object.
    #[error("client returned an error: {0}")]
    ClientError(String),

    /// The client's answer was not a well-formed elicitation result.
    #[error("malformed elicitation result: {0}")]
    Malformed(String),
}

/// How one connection writes a server-initiated JSON-RPC message.
///
/// Implemented per transport over the channel that transport already owns.
/// `send` is fallible because every real channel can be closed by the peer
/// disconnecting between the lookup and the write.
#[async_trait]
pub trait OutboundSink: Send + Sync {
    /// Write one JSON-RPC message to the client.
    async fn send(&self, message: Value) -> Result<(), SessionError>;
}

/// One live MCP connection, and the requests this server has outstanding on it.
pub struct McpSession {
    /// Transport-minted session id — the value [`stamp_session_id`] publishes.
    ///
    /// [`stamp_session_id`]: crate::server::transport
    id: String,
    sink: Arc<dyn OutboundSink>,
    /// Server request id to the waiter for its answer.
    pending: Mutex<HashMap<String, oneshot::Sender<Value>>>,
    /// `capabilities` from the client's `initialize`, once seen.
    ///
    /// `None` means `initialize` has not arrived (or carried no capabilities).
    /// Absence is treated as "not supported": a client that never declared
    /// elicitation cannot be assumed to answer an elicitation request.
    client_capabilities: Mutex<Option<Value>>,
    elicit_timeout: Duration,
}

impl McpSession {
    /// Create a session bound to `id` writing through `sink`.
    pub fn new(id: impl Into<String>, sink: Arc<dyn OutboundSink>) -> Self {
        Self {
            id: id.into(),
            sink,
            pending: Mutex::new(HashMap::new()),
            client_capabilities: Mutex::new(None),
            elicit_timeout: DEFAULT_ELICIT_TIMEOUT,
        }
    }

    /// Override how long server-initiated requests wait for an answer.
    pub fn with_elicit_timeout(mut self, timeout: Duration) -> Self {
        self.elicit_timeout = timeout;
        self
    }

    /// The transport-minted session id.
    pub fn id(&self) -> &str {
        &self.id
    }

    /// Record the `capabilities` object from the client's `initialize`.
    pub fn set_client_capabilities(&self, capabilities: Value) {
        *lock(&self.client_capabilities) = Some(capabilities);
    }

    /// Whether the client advertised the `elicitation` capability.
    ///
    /// Per the MCP schema the value is an object whose presence — not its
    /// contents — is the declaration, so any non-null value counts. A session
    /// that has not seen `initialize` reports `false`.
    pub fn supports_elicitation(&self) -> bool {
        lock(&self.client_capabilities)
            .as_ref()
            .and_then(|caps| caps.get("elicitation"))
            .is_some_and(|v| !v.is_null())
    }

    /// Try to consume `message` as the answer to an outstanding server request.
    ///
    /// Returns `true` when it was one, in which case the waiter has been woken
    /// and the caller must **not** pass the message to the [`McpHandler`].
    /// Returns `false` for anything else, including a response-shaped message
    /// whose id this session never issued.
    ///
    /// [`McpHandler`]: crate::server::transport::McpHandler
    pub fn try_take_response(&self, message: &Value) -> bool {
        // A response has an id and no method. Checking `method` first is what
        // keeps a request carrying an id that happens to collide with a
        // server request id from being swallowed as an answer.
        if message.get("method").is_some() {
            return false;
        }
        let Some(id) = message.get("id").and_then(response_id_key) else {
            return false;
        };
        let Some(waiter) = lock(&self.pending).remove(&id) else {
            return false;
        };
        // A closed receiver means the waiter timed out or was cancelled
        // between the lookup and now. The message was still ours, so it is
        // consumed either way — forwarding it to the handler would only
        // produce a spurious "invalid request".
        let _ = waiter.send(message.clone());
        true
    }

    /// Fail every outstanding request on this session.
    ///
    /// Called when the connection ends: dropping the senders wakes each
    /// waiter with [`SessionError::Closed`] instead of leaving it to time out
    /// long after the client is gone.
    pub fn close(&self) {
        lock(&self.pending).clear();
    }

    /// Number of requests currently awaiting an answer. Test/observability aid.
    pub fn pending_count(&self) -> usize {
        lock(&self.pending).len()
    }

    /// Send a server-initiated request and await the client's answer.
    async fn request(&self, method: &str, params: Value) -> Result<Value, SessionError> {
        let id = format!("apcore-mcp-{}", uuid::Uuid::new_v4());
        let (tx, rx) = oneshot::channel();

        // Registered before the write so an answer that races back in cannot
        // find an empty table. Dropped on every exit path below, including
        // the `?` on a failed send, so the table cannot leak an entry.
        let _pending = PendingGuard::register(self, id.clone(), tx);

        self.sink
            .send(serde_json::json!({
                "jsonrpc": "2.0",
                "id": id,
                "method": method,
                "params": params,
            }))
            .await?;

        match tokio::time::timeout(self.elicit_timeout, rx).await {
            Ok(Ok(response)) => Ok(response),
            // Sender dropped: the session was torn down under us.
            Ok(Err(_)) => Err(SessionError::Closed),
            Err(_) => Err(SessionError::Timeout(self.elicit_timeout)),
        }
    }
}

#[async_trait]
impl SessionHandle for McpSession {
    async fn elicit_form(
        &self,
        message: &str,
        requested_schema: &Value,
    ) -> Result<ElicitResult, Box<dyn std::error::Error + Send + Sync>> {
        if !self.supports_elicitation() {
            return Err(SessionError::ElicitationUnsupported.into());
        }

        let response = self
            .request(
                "elicitation/create",
                serde_json::json!({
                    "message": message,
                    "requestedSchema": requested_schema,
                }),
            )
            .await?;

        if let Some(error) = response.get("error") {
            let detail = error
                .get("message")
                .and_then(Value::as_str)
                .unwrap_or("unspecified");
            return Err(SessionError::ClientError(detail.to_string()).into());
        }

        let result = response.get("result").ok_or_else(|| {
            SessionError::Malformed("response has neither result nor error".into())
        })?;

        serde_json::from_value::<ElicitResult>(result.clone())
            .map_err(|e| SessionError::Malformed(e.to_string()).into())
    }
}

/// RAII entry in [`McpSession::pending`].
///
/// The registration and its removal are a matched pair across four exit paths
/// (send failure, answer, timeout, teardown). `Drop` is what makes them one
/// statement instead of four, and what makes the removal survive a panic on
/// the await.
struct PendingGuard<'a> {
    session: &'a McpSession,
    id: String,
}

impl<'a> PendingGuard<'a> {
    fn register(session: &'a McpSession, id: String, waiter: oneshot::Sender<Value>) -> Self {
        lock(&session.pending).insert(id.clone(), waiter);
        Self { session, id }
    }
}

impl Drop for PendingGuard<'_> {
    fn drop(&mut self) {
        lock(&self.session.pending).remove(&self.id);
    }
}

/// Registry of live sessions, keyed by transport-minted session id.
///
/// Shared between the transport (which registers and deregisters connections)
/// and [`ExecutionRouter`], which looks a session up by the id stamped onto
/// the inbound message. Cloneable via `Arc` by the holder, never by value.
///
/// [`ExecutionRouter`]: crate::server::router::ExecutionRouter
#[derive(Default)]
pub struct SessionRegistry {
    sessions: Mutex<HashMap<String, Arc<McpSession>>>,
}

impl SessionRegistry {
    /// Create an empty registry.
    pub fn new() -> Self {
        Self::default()
    }

    /// Register `session` under its own id, replacing any prior entry.
    pub fn insert(&self, session: Arc<McpSession>) {
        lock(&self.sessions).insert(session.id().to_string(), session);
    }

    /// Look up a live session.
    pub fn get(&self, session_id: &str) -> Option<Arc<McpSession>> {
        lock(&self.sessions).get(session_id).cloned()
    }

    /// Deregister a session and fail anything it still has outstanding.
    pub fn remove(&self, session_id: &str) {
        let removed = lock(&self.sessions).remove(session_id);
        if let Some(session) = removed {
            session.close();
        }
    }

    /// Number of live sessions. Test/observability aid.
    pub fn len(&self) -> usize {
        lock(&self.sessions).len()
    }

    /// Whether any session is live.
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

/// Normalise a JSON-RPC id into the pending table's key type.
///
/// JSON-RPC permits a string or a number. Both are keyed by their textual
/// form so `1` and `"1"` cannot address separate entries — ids minted here are
/// always strings, so a numeric id can only ever miss.
fn response_id_key(id: &Value) -> Option<String> {
    match id {
        Value::String(s) => Some(s.clone()),
        Value::Number(n) => Some(n.to_string()),
        _ => None,
    }
}

/// Lock a mutex, recovering from poisoning.
///
/// Same reasoning as the transport's session registry: the guarded values are
/// plain map entries, and a panic mid-mutation cannot leave a `HashMap` in a
/// state a later read misinterprets. Refusing every subsequent elicitation
/// because one call panicked would be the larger failure.
fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Weak;

    use crate::helpers::ElicitAction;

    /// Sink that records what the server sent and answers it immediately.
    ///
    /// Answering from inside `send` is deliberate: the pending entry is
    /// registered *before* the write, so a reply that races back this fast
    /// must still find its waiter. A registration ordered after the write
    /// would drop the answer on the floor, and this is the test that would
    /// catch it.
    struct AutoReplySink {
        session: Mutex<Weak<McpSession>>,
        sent: Mutex<Vec<Value>>,
        reply: Value,
    }

    impl AutoReplySink {
        fn new(reply: Value) -> Arc<Self> {
            Arc::new(Self {
                session: Mutex::new(Weak::new()),
                sent: Mutex::new(Vec::new()),
                reply,
            })
        }

        fn bind(&self, session: &Arc<McpSession>) {
            *lock(&self.session) = Arc::downgrade(session);
        }

        fn sent(&self) -> Vec<Value> {
            lock(&self.sent).clone()
        }
    }

    #[async_trait]
    impl OutboundSink for AutoReplySink {
        async fn send(&self, message: Value) -> Result<(), SessionError> {
            lock(&self.sent).push(message.clone());
            let session = lock(&self.session).upgrade();
            if let Some(session) = session {
                let id = message.get("id").cloned().unwrap_or(Value::Null);
                session.try_take_response(&serde_json::json!({
                    "jsonrpc": "2.0",
                    "id": id,
                    "result": self.reply,
                }));
            }
            Ok(())
        }
    }

    /// Sink that accepts the write and never answers.
    struct SilentSink {
        sent: Mutex<Vec<Value>>,
    }

    impl SilentSink {
        fn new() -> Arc<Self> {
            Arc::new(Self {
                sent: Mutex::new(Vec::new()),
            })
        }

        fn sent(&self) -> Vec<Value> {
            lock(&self.sent).clone()
        }
    }

    #[async_trait]
    impl OutboundSink for SilentSink {
        async fn send(&self, message: Value) -> Result<(), SessionError> {
            lock(&self.sent).push(message);
            Ok(())
        }
    }

    /// Sink whose channel is already gone.
    struct DeadSink;

    #[async_trait]
    impl OutboundSink for DeadSink {
        async fn send(&self, _message: Value) -> Result<(), SessionError> {
            Err(SessionError::Send("closed".to_string()))
        }
    }

    fn elicitation_capable() -> Value {
        serde_json::json!({ "elicitation": {} })
    }

    // -- capability gating ---------------------------------------------------

    #[tokio::test]
    async fn elicit_is_refused_when_the_client_never_declared_the_capability() {
        let sink = SilentSink::new();
        let session = McpSession::new("s1", Arc::clone(&sink) as Arc<dyn OutboundSink>);

        let error = session
            .elicit_form("approve?", &serde_json::json!({}))
            .await
            .expect_err("a client that never advertised elicitation must not be prompted");

        assert!(
            error.to_string().contains("elicitation capability"),
            "the refusal must name the missing capability: {error}"
        );
        assert!(
            sink.sent().is_empty(),
            "nothing may be written to a client that cannot answer it"
        );
    }

    #[tokio::test]
    async fn elicit_is_refused_when_initialize_declared_other_capabilities_only() {
        // `capabilities: {}` is a real declaration, and it declares no
        // elicitation. Treating "initialize was seen" as "elicitation is
        // supported" would pass the test above and fail here.
        let sink = SilentSink::new();
        let session = McpSession::new("s1", Arc::clone(&sink) as Arc<dyn OutboundSink>);
        session.set_client_capabilities(serde_json::json!({ "roots": {} }));

        assert!(!session.supports_elicitation());
        assert!(session
            .elicit_form("approve?", &serde_json::json!({}))
            .await
            .is_err());
        assert!(sink.sent().is_empty());
    }

    // -- the round trip ------------------------------------------------------

    #[tokio::test]
    async fn elicit_sends_a_conforming_request_and_returns_the_client_answer() {
        let sink = AutoReplySink::new(serde_json::json!({
            "action": "accept",
            "content": { "confirm": true },
        }));
        let session = Arc::new(McpSession::new(
            "s1",
            Arc::clone(&sink) as Arc<dyn OutboundSink>,
        ));
        sink.bind(&session);
        session.set_client_capabilities(elicitation_capable());

        let schema = serde_json::json!({
            "type": "object",
            "properties": { "confirm": { "type": "boolean" } },
        });
        let result = session
            .elicit_form("approve demo.gated?", &schema)
            .await
            .expect("the client answered");

        assert_eq!(result.action, ElicitAction::Accept);
        assert_eq!(
            result.content,
            Some(serde_json::json!({ "confirm": true })),
            "the client's form data must survive the round trip"
        );

        let sent = sink.sent();
        assert_eq!(sent.len(), 1, "exactly one request: {sent:?}");
        let request = &sent[0];
        assert_eq!(request["jsonrpc"], "2.0");
        assert_eq!(
            request["method"], "elicitation/create",
            "the MCP method name is what makes this reachable by a real client"
        );
        assert_eq!(request["params"]["message"], "approve demo.gated?");
        assert_eq!(request["params"]["requestedSchema"], schema);
        assert!(
            request["id"].is_string(),
            "a server-initiated request must carry an id to correlate on: {request}"
        );
        assert_eq!(
            session.pending_count(),
            0,
            "the pending entry must be gone once the call returns"
        );
    }

    #[tokio::test]
    async fn elicit_reports_a_declining_client_without_inventing_an_answer() {
        let sink = AutoReplySink::new(serde_json::json!({ "action": "decline" }));
        let session = Arc::new(McpSession::new(
            "s1",
            Arc::clone(&sink) as Arc<dyn OutboundSink>,
        ));
        sink.bind(&session);
        session.set_client_capabilities(elicitation_capable());

        let result = session
            .elicit_form("approve?", &serde_json::json!({}))
            .await
            .expect("decline is an answer, not a failure");

        assert_eq!(result.action, ElicitAction::Decline);
        assert_eq!(result.content, None);
    }

    #[tokio::test]
    async fn each_request_gets_its_own_id() {
        let sink = AutoReplySink::new(serde_json::json!({ "action": "accept" }));
        let session = Arc::new(McpSession::new(
            "s1",
            Arc::clone(&sink) as Arc<dyn OutboundSink>,
        ));
        sink.bind(&session);
        session.set_client_capabilities(elicitation_capable());

        session
            .elicit_form("first", &serde_json::json!({}))
            .await
            .expect("first");
        session
            .elicit_form("second", &serde_json::json!({}))
            .await
            .expect("second");

        let sent = sink.sent();
        assert_ne!(
            sent[0]["id"], sent[1]["id"],
            "reusing an id would let one answer complete the wrong request"
        );
    }

    // -- error propagation ---------------------------------------------------

    #[tokio::test]
    async fn a_client_error_response_fails_the_call_rather_than_approving_it() {
        let sink = AutoReplySink::new(Value::Null);
        let session = Arc::new(McpSession::new(
            "s1",
            Arc::clone(&sink) as Arc<dyn OutboundSink>,
        ));
        sink.bind(&session);
        session.set_client_capabilities(elicitation_capable());

        // Rebuild the reply as a JSON-RPC error rather than a result.
        struct ErroringSink {
            session: Mutex<Weak<McpSession>>,
        }
        #[async_trait]
        impl OutboundSink for ErroringSink {
            async fn send(&self, message: Value) -> Result<(), SessionError> {
                if let Some(session) = lock(&self.session).upgrade() {
                    session.try_take_response(&serde_json::json!({
                        "jsonrpc": "2.0",
                        "id": message.get("id").cloned().unwrap_or(Value::Null),
                        "error": { "code": -32601, "message": "elicitation not implemented" },
                    }));
                }
                Ok(())
            }
        }
        let erroring = Arc::new(ErroringSink {
            session: Mutex::new(Weak::new()),
        });
        let session = Arc::new(McpSession::new(
            "s2",
            Arc::clone(&erroring) as Arc<dyn OutboundSink>,
        ));
        *lock(&erroring.session) = Arc::downgrade(&session);
        session.set_client_capabilities(elicitation_capable());

        let error = session
            .elicit_form("approve?", &serde_json::json!({}))
            .await
            .expect_err("an error response is not an approval");
        assert!(
            error.to_string().contains("elicitation not implemented"),
            "the client's reason must be surfaced: {error}"
        );
        assert_eq!(session.pending_count(), 0);
    }

    #[tokio::test]
    async fn a_malformed_result_fails_the_call() {
        let sink = AutoReplySink::new(serde_json::json!({ "notAnAction": true }));
        let session = Arc::new(McpSession::new(
            "s1",
            Arc::clone(&sink) as Arc<dyn OutboundSink>,
        ));
        sink.bind(&session);
        session.set_client_capabilities(elicitation_capable());

        let error = session
            .elicit_form("approve?", &serde_json::json!({}))
            .await
            .expect_err("a result without an action is not an approval");
        assert!(
            error.to_string().contains("malformed"),
            "unexpected error: {error}"
        );
    }

    #[tokio::test]
    async fn a_failed_write_does_not_leak_a_pending_entry() {
        let session = McpSession::new("s1", Arc::new(DeadSink));
        session.set_client_capabilities(elicitation_capable());

        assert!(session
            .elicit_form("approve?", &serde_json::json!({}))
            .await
            .is_err());
        assert_eq!(
            session.pending_count(),
            0,
            "the RAII guard must clear the entry on the send-failure path too"
        );
    }

    #[tokio::test(start_paused = true)]
    async fn a_silent_client_times_out_and_leaves_no_pending_entry() {
        let sink = SilentSink::new();
        let session = McpSession::new("s1", Arc::clone(&sink) as Arc<dyn OutboundSink>)
            .with_elicit_timeout(Duration::from_secs(30));
        session.set_client_capabilities(elicitation_capable());

        let error = session
            .elicit_form("approve?", &serde_json::json!({}))
            .await
            .expect_err("a client that never answers must not pin the call forever");
        assert!(
            error.to_string().contains("did not answer"),
            "unexpected error: {error}"
        );
        assert_eq!(session.pending_count(), 0);
        assert_eq!(sink.sent().len(), 1, "the request was still written once");
    }

    // -- response interception ------------------------------------------------

    #[tokio::test]
    async fn a_message_with_a_method_is_never_taken_as_a_response() {
        // A client request that happens to reuse a server request id must
        // still reach the handler. Keying on the id alone would swallow it.
        let sink = SilentSink::new();
        let session = Arc::new(McpSession::new(
            "s1",
            Arc::clone(&sink) as Arc<dyn OutboundSink>,
        ));
        session.set_client_capabilities(elicitation_capable());

        let waiting = Arc::clone(&session);
        let call = tokio::spawn(async move {
            waiting
                .elicit_form("approve?", &serde_json::json!({}))
                .await
                .map(|r| r.action)
        });
        let id = loop {
            if let Some(request) = sink.sent().first() {
                break request["id"].clone();
            }
            tokio::task::yield_now().await;
        };

        assert!(
            !session.try_take_response(&serde_json::json!({
                "jsonrpc": "2.0",
                "id": id,
                "method": "tools/call",
                "params": {},
            })),
            "a request is not a response, whatever its id"
        );
        assert_eq!(session.pending_count(), 1, "the waiter is still waiting");

        session.close();
        assert!(tokio::time::timeout(Duration::from_secs(5), call)
            .await
            .expect("close must wake the call")
            .expect("join")
            .is_err());
    }

    #[tokio::test]
    async fn an_unknown_id_is_passed_through_rather_than_swallowed() {
        let sink = SilentSink::new();
        let session = McpSession::new("s1", Arc::clone(&sink) as Arc<dyn OutboundSink>);

        assert!(
            !session.try_take_response(&serde_json::json!({
                "jsonrpc": "2.0",
                "id": "never-issued",
                "result": {},
            })),
            "an id this session never minted belongs to the handler's error path"
        );
    }

    #[tokio::test]
    async fn closing_the_session_wakes_a_waiting_call() {
        let sink = SilentSink::new();
        let session = Arc::new(
            McpSession::new("s1", Arc::clone(&sink) as Arc<dyn OutboundSink>)
                .with_elicit_timeout(Duration::from_secs(3600)),
        );
        session.set_client_capabilities(elicitation_capable());

        let waiting = Arc::clone(&session);
        let call = tokio::spawn(async move {
            waiting
                .elicit_form("approve?", &serde_json::json!({}))
                .await
                .map(|r| r.action)
        });
        while sink.sent().is_empty() {
            tokio::task::yield_now().await;
        }

        session.close();

        // Bounded, so a regression fails in seconds rather than hanging for
        // the hour-long elicit timeout this test deliberately configures.
        let error = tokio::time::timeout(Duration::from_secs(5), call)
            .await
            .expect("close must wake the call, not leave it on its timeout")
            .expect("join")
            .expect_err("a disconnect must not wait out the timeout");
        assert!(
            error.to_string().contains("session closed"),
            "unexpected error: {error}"
        );
    }

    // -- registry -------------------------------------------------------------

    #[tokio::test]
    async fn the_registry_resolves_a_session_by_id_and_forgets_it_on_removal() {
        let registry = SessionRegistry::new();
        assert!(registry.is_empty());

        let session = Arc::new(McpSession::new("s1", Arc::new(DeadSink)));
        registry.insert(Arc::clone(&session));

        assert_eq!(registry.len(), 1);
        assert!(registry.get("s1").is_some());
        assert!(
            registry.get("s2").is_none(),
            "one connection's id must not resolve another's session"
        );

        registry.remove("s1");
        assert!(registry.get("s1").is_none());
        assert!(registry.is_empty());
    }

    #[tokio::test]
    async fn removing_a_session_from_the_registry_wakes_its_waiters() {
        let sink = SilentSink::new();
        let registry = Arc::new(SessionRegistry::new());
        let session = Arc::new(
            McpSession::new("s1", Arc::clone(&sink) as Arc<dyn OutboundSink>)
                .with_elicit_timeout(Duration::from_secs(3600)),
        );
        session.set_client_capabilities(elicitation_capable());
        registry.insert(Arc::clone(&session));

        let waiting = Arc::clone(&session);
        let call = tokio::spawn(async move {
            waiting
                .elicit_form("approve?", &serde_json::json!({}))
                .await
                .map(|r| r.action)
        });
        while sink.sent().is_empty() {
            tokio::task::yield_now().await;
        }

        registry.remove("s1");

        let outcome = tokio::time::timeout(Duration::from_secs(5), call)
            .await
            .expect("deregistration must wake the call, not leave it on its timeout")
            .expect("join");
        assert!(
            outcome.is_err(),
            "deregistration is what a transport does on disconnect; it must \
             wake the call rather than leave it to time out"
        );
    }
}
