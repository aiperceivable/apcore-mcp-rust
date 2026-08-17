//! Crate-local registry that lets an [`ApprovalHandler`] reach the router's
//! per-call [`ElicitCallback`].
//!
//! # Why this exists
//!
//! apcore hands an [`ApprovalHandler`] an `ApprovalRequest` whose only channel
//! back to the transport is `context.data`, typed
//! `Arc<RwLock<HashMap<String, serde_json::Value>>>`. An `ElicitCallback` is a
//! `Box<dyn Fn(..) -> Pin<Box<dyn Future<..>>>>`; no closure is a
//! `serde_json::Value`, so the callback itself can never be put there. Before
//! this module the router wrote the string `"available"` instead, which told a
//! handler that elicitation *existed* while giving it no way to invoke it —
//! so [`ElicitationApprovalHandler`] rejected every gated call on a
//! CLI-launched server (apexe#29).
//!
//! apcore-python has no such problem: `Context.data` holds arbitrary Python
//! objects, so its adapter reads the callback straight back out
//! (`adapters/approval.py`). This is a Rust-only limitation, not a cross-SDK
//! constraint, and it is fixed here without an apcore change.
//!
//! # The handle indirection
//!
//! A `String` *is* a `serde_json::Value`. So the router registers the callback
//! here, receives an [`ElicitGuard`] carrying an opaque id, and writes only
//! that id into `context.data`. The handler reads the id back and exchanges it
//! for the callback. The map is process-local and never crosses the wire.
//!
//! # Lifetime
//!
//! [`ElicitGuard`] is RAII: the entry is removed in `Drop`, so it lives exactly
//! as long as the guard the router holds for the tool call. Drop runs on the
//! unwind path too, so a panicking tool call deregisters just as a returning
//! one does (`test_guard_deregisters_when_the_scope_panics` pins this). The registry
//! therefore cannot grow without bound, and a stale id cannot resolve to a
//! callback belonging to a finished call.
//!
//! [`ApprovalHandler`]: apcore::approval::ApprovalHandler
//! [`ElicitationApprovalHandler`]: crate::adapters::ElicitationApprovalHandler

use std::collections::HashMap;
use std::sync::{Arc, Mutex, MutexGuard, OnceLock};

use uuid::Uuid;

use crate::helpers::ElicitCallback;

/// Shared handle to a registered callback.
///
/// `Arc` rather than a borrow because the handler invokes the callback across
/// an `.await`, long after any lock on the registry must have been released.
pub(crate) type SharedElicitCallback = Arc<ElicitCallback>;

type RegistryMap = HashMap<String, SharedElicitCallback>;

/// Process-wide registry of live elicit callbacks, keyed by call id.
fn registry() -> &'static Mutex<RegistryMap> {
    static REGISTRY: OnceLock<Mutex<RegistryMap>> = OnceLock::new();
    REGISTRY.get_or_init(|| Mutex::new(HashMap::new()))
}

/// Lock the registry, recovering from poisoning.
///
/// Every critical section in this module is a single `HashMap` operation on
/// data owned entirely by the map — no user code, no callback invocation, and
/// no `.await` runs while the lock is held. A panic while holding it is
/// therefore not reachable through this module's own code, and if some future
/// edit made it reachable the map would still be structurally intact.
///
/// Recovering is the safe choice rather than the lazy one: propagating
/// poisoning would turn one unrelated panic into a permanent outage of the
/// approval path for the life of the process, failing every subsequent
/// elicitation and leaking every entry that was live at the time.
fn lock_registry() -> MutexGuard<'static, RegistryMap> {
    registry().lock().unwrap_or_else(|poisoned| {
        tracing::warn!("elicit registry mutex was poisoned; recovering the map");
        poisoned.into_inner()
    })
}

/// RAII handle owning one registered [`ElicitCallback`].
///
/// Hold it for exactly the span of the tool call whose context carries
/// [`id`](Self::id). Dropping it deregisters the callback.
#[derive(Debug)]
pub(crate) struct ElicitGuard {
    id: String,
}

impl ElicitGuard {
    /// The opaque call id to write into `Context.data`.
    pub(crate) fn id(&self) -> &str {
        &self.id
    }
}

impl Drop for ElicitGuard {
    fn drop(&mut self) {
        lock_registry().remove(&self.id);
    }
}

/// Register `callback` and return the guard that owns it.
///
/// The id is a v4 uuid: unguessable, so an id observed in one context cannot
/// be used to guess a live one, and collision-free across concurrent calls.
pub(crate) fn register(callback: ElicitCallback) -> ElicitGuard {
    let id = Uuid::new_v4().to_string();
    lock_registry().insert(id.clone(), Arc::new(callback));
    ElicitGuard { id }
}

/// Resolve a call id back to its callback, or `None` when the call has ended.
pub(crate) fn lookup(id: &str) -> Option<SharedElicitCallback> {
    lock_registry().get(id).map(Arc::clone)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::helpers::{ElicitAction, ElicitResult};

    /// A callback that reports which label it was built with, so a lookup
    /// returning the WRONG callback is distinguishable from one returning none.
    fn labelled_callback(label: &'static str) -> ElicitCallback {
        Box::new(move |_message, _schema| {
            Box::pin(async move {
                Some(ElicitResult {
                    action: ElicitAction::Accept,
                    content: Some(serde_json::json!({ "label": label })),
                })
            })
        })
    }

    async fn invoke(callback: &SharedElicitCallback) -> String {
        let result = callback("prompt".to_string(), None)
            .await
            .expect("callback must answer");
        result
            .content
            .and_then(|c| c.get("label").and_then(|v| v.as_str()).map(str::to_owned))
            .expect("callback must report its label")
    }

    #[tokio::test]
    async fn test_lookup_returns_the_registered_callback() {
        let guard = register(labelled_callback("first"));
        let found = lookup(guard.id()).expect("a live id must resolve");
        assert_eq!(invoke(&found).await, "first");
    }

    #[tokio::test]
    async fn test_lookup_distinguishes_concurrent_registrations() {
        // Two live calls at once must not resolve to each other's callback —
        // the failure that would let one client's prompt answer another's.
        let first = register(labelled_callback("first"));
        let second = register(labelled_callback("second"));
        assert_ne!(first.id(), second.id(), "ids must be unique");

        let found_first = lookup(first.id()).expect("first must resolve");
        let found_second = lookup(second.id()).expect("second must resolve");
        assert_eq!(invoke(&found_first).await, "first");
        assert_eq!(invoke(&found_second).await, "second");
    }

    #[test]
    fn test_lookup_returns_none_for_an_unknown_id() {
        assert!(lookup("no-such-call-id").is_none());
    }

    #[test]
    fn test_guard_deregisters_on_drop() {
        let id = {
            let guard = register(labelled_callback("scoped"));
            let id = guard.id().to_string();
            assert!(lookup(&id).is_some(), "must resolve while the guard lives");
            id
        };
        assert!(
            lookup(&id).is_none(),
            "the entry must not outlive its guard: a stale id resolving to a \
             live callback is how one call reaches another call's client"
        );
    }

    #[test]
    fn test_guard_deregisters_when_the_scope_panics() {
        // The tool call this guard scopes runs third-party module code. If a
        // panic could skip deregistration the registry would leak an entry per
        // panicking call, and the id would stay resolvable afterwards. Drop
        // runs during unwind, so it does not — asserted rather than assumed.
        let captured = std::sync::Arc::new(std::sync::Mutex::new(String::new()));
        let sink = std::sync::Arc::clone(&captured);
        let outcome = std::panic::catch_unwind(move || {
            let guard = register(labelled_callback("panicking"));
            *sink.lock().expect("sink lock") = guard.id().to_string();
            panic!("the tool call panicked");
        });

        assert!(outcome.is_err(), "the scope must actually have panicked");
        let leaked_id = captured.lock().expect("sink lock").clone();
        assert!(!leaked_id.is_empty(), "the guard must have been registered");
        assert!(
            lookup(&leaked_id).is_none(),
            "a panicking tool call must still deregister its callback"
        );
    }
}
