//! ApprovalStore: pluggable persistence trait for Phase B approval polling.
//!
//! Users implement `ApprovalStore` (Redis, DB, etc.).
//! `InMemoryApprovalStore` is provided for testing and local development only.

use async_trait::async_trait;
use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::sync::{Mutex, Notify};

/// Convenience alias for the store's fallible operations.
type StoreResult<T> = Result<T, Box<dyn std::error::Error + Send + Sync>>;

/// Status of an approval record.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ApprovalStatus {
    Pending,
    Approved,
    Rejected,
}

impl std::fmt::Display for ApprovalStatus {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Pending => write!(f, "pending"),
            Self::Approved => write!(f, "approved"),
            Self::Rejected => write!(f, "rejected"),
        }
    }
}

/// A snapshot of an approval record (fields available to callers).
#[derive(Debug, Clone)]
pub struct ApprovalRecord {
    pub status: ApprovalStatus,
    pub module_id: String,
    pub reason: Option<String>,
}

/// Pluggable storage for Phase B approval state.
#[async_trait]
pub trait ApprovalStore: Send + Sync + 'static {
    /// Persist a new pending approval record.
    async fn save_pending(
        &self,
        approval_id: &str,
        module_id: &str,
        arguments: &serde_json::Value,
    ) -> StoreResult<()>;

    /// Return the current record, or None if approval_id is unknown.
    async fn get_result(&self, approval_id: &str) -> StoreResult<Option<ApprovalRecord>>;

    /// Mark approval_id as approved or rejected.
    /// Returns false if the id is unknown or already resolved.
    async fn resolve(
        &self,
        approval_id: &str,
        approved: bool,
        reason: Option<String>,
    ) -> StoreResult<bool>;
}

// ── InMemoryApprovalStore ────────────────────────────────────────────────────

struct InternalRecord {
    status: ApprovalStatus,
    module_id: String,
    reason: Option<String>,
    created_at: Instant,
    resolved_at: Option<Instant>,
    notify: Arc<Notify>,
}

/// In-process approval store for testing and local development.
///
/// NOT suitable for production.
///
/// Memory management: resolved records expire after `resolved_ttl`,
/// pending records after `pending_ttl`. A background sweep task (started
/// via `start_sweep`) handles records whose timeouts were missed.
pub struct InMemoryApprovalStore {
    records: Arc<Mutex<HashMap<String, InternalRecord>>>,
    resolved_ttl: Duration,
    pending_ttl: Duration,
    max_records: usize,
}

impl InMemoryApprovalStore {
    pub fn new() -> Self {
        Self::with_options(Duration::from_secs(120), Duration::from_secs(3600), 10_000)
    }

    pub fn with_options(resolved_ttl: Duration, pending_ttl: Duration, max_records: usize) -> Self {
        Self {
            records: Arc::new(Mutex::new(HashMap::new())),
            resolved_ttl,
            pending_ttl,
            max_records,
        }
    }

    /// Start a background sweep task. Call once from async context.
    pub fn start_sweep(&self) -> tokio::task::JoinHandle<()> {
        let records = Arc::clone(&self.records);
        let pending_ttl = self.pending_ttl;
        let resolved_ttl = self.resolved_ttl;
        tokio::spawn(async move {
            let interval = Duration::from_secs(300);
            loop {
                tokio::time::sleep(interval).await;
                let mut map = records.lock().await;
                let now = Instant::now();
                map.retain(|_, rec| {
                    if rec.status == ApprovalStatus::Pending {
                        now.duration_since(rec.created_at) <= pending_ttl
                    } else {
                        rec.resolved_at
                            .map(|t| now.duration_since(t) <= resolved_ttl)
                            .unwrap_or(true)
                    }
                });
            }
        })
    }

    async fn evict_oldest_pending(map: &mut HashMap<String, InternalRecord>) {
        let oldest = map
            .iter()
            .filter(|(_, r)| r.status == ApprovalStatus::Pending)
            .min_by_key(|(_, r)| r.created_at)
            .map(|(id, _)| id.clone());
        if let Some(id) = oldest {
            map.remove(&id);
        }
    }
}

impl Default for InMemoryApprovalStore {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl ApprovalStore for InMemoryApprovalStore {
    async fn save_pending(
        &self,
        approval_id: &str,
        module_id: &str,
        _arguments: &serde_json::Value,
    ) -> StoreResult<()> {
        let mut map = self.records.lock().await;
        if map.len() >= self.max_records {
            Self::evict_oldest_pending(&mut map).await;
        }
        map.insert(
            approval_id.to_string(),
            InternalRecord {
                status: ApprovalStatus::Pending,
                module_id: module_id.to_string(),
                reason: None,
                created_at: Instant::now(),
                resolved_at: None,
                notify: Arc::new(Notify::new()),
            },
        );

        // Schedule TTL cleanup
        let records = Arc::clone(&self.records);
        let id = approval_id.to_string();
        let ttl = self.pending_ttl;
        tokio::spawn(async move {
            tokio::time::sleep(ttl).await;
            records.lock().await.remove(&id);
        });

        Ok(())
    }

    async fn get_result(&self, approval_id: &str) -> StoreResult<Option<ApprovalRecord>> {
        let map = self.records.lock().await;
        Ok(map.get(approval_id).map(|r| ApprovalRecord {
            status: r.status.clone(),
            module_id: r.module_id.clone(),
            reason: r.reason.clone(),
        }))
    }

    async fn resolve(
        &self,
        approval_id: &str,
        approved: bool,
        reason: Option<String>,
    ) -> StoreResult<bool> {
        let mut map = self.records.lock().await;
        let Some(rec) = map.get_mut(approval_id) else {
            return Ok(false);
        };
        if rec.status != ApprovalStatus::Pending {
            return Ok(false);
        }
        rec.status = if approved {
            ApprovalStatus::Approved
        } else {
            ApprovalStatus::Rejected
        };
        rec.reason = reason;
        rec.resolved_at = Some(Instant::now());
        rec.notify.notify_waiters();

        // Schedule resolved-TTL cleanup
        let records = Arc::clone(&self.records);
        let id = approval_id.to_string();
        let ttl = self.resolved_ttl;
        drop(map); // release lock before spawning
        tokio::spawn(async move {
            tokio::time::sleep(ttl).await;
            records.lock().await.remove(&id);
        });

        Ok(true)
    }
}

impl InMemoryApprovalStore {
    /// Wait until an approval is resolved or timeout. Helper for tests.
    pub async fn wait_for_resolution(
        &self,
        approval_id: &str,
        timeout: Duration,
    ) -> Option<ApprovalRecord> {
        let notify = {
            let map = self.records.lock().await;
            map.get(approval_id)
                .filter(|r| r.status == ApprovalStatus::Pending)
                .map(|r| Arc::clone(&r.notify))
        };
        if let Some(n) = notify {
            let _ = tokio::time::timeout(timeout, n.notified()).await;
        }
        self.get_result(approval_id).await.ok().flatten()
    }
}

// ===========================================================================
// Tests
// ===========================================================================

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[tokio::test]
    async fn save_pending_then_get_result_returns_pending() {
        let store = InMemoryApprovalStore::new();
        store
            .save_pending("abc", "my.module", &json!({"x": 1}))
            .await
            .unwrap();
        let record = store.get_result("abc").await.unwrap().unwrap();
        assert_eq!(record.status, ApprovalStatus::Pending);
        assert_eq!(record.module_id, "my.module");
        assert!(record.reason.is_none());
    }

    #[tokio::test]
    async fn resolve_approved_returns_approved_status() {
        let store = InMemoryApprovalStore::new();
        store
            .save_pending("id1", "mod.a", &json!({}))
            .await
            .unwrap();
        let resolved = store.resolve("id1", true, None).await.unwrap();
        assert!(resolved);
        let record = store.get_result("id1").await.unwrap().unwrap();
        assert_eq!(record.status, ApprovalStatus::Approved);
        assert!(record.reason.is_none());
    }

    #[tokio::test]
    async fn resolve_rejected_with_reason() {
        let store = InMemoryApprovalStore::new();
        store
            .save_pending("id2", "mod.b", &json!({}))
            .await
            .unwrap();
        let resolved = store
            .resolve("id2", false, Some("not allowed".to_string()))
            .await
            .unwrap();
        assert!(resolved);
        let record = store.get_result("id2").await.unwrap().unwrap();
        assert_eq!(record.status, ApprovalStatus::Rejected);
        assert_eq!(record.reason.as_deref(), Some("not allowed"));
    }

    #[tokio::test]
    async fn resolve_unknown_returns_false() {
        let store = InMemoryApprovalStore::new();
        let resolved = store.resolve("nonexistent", true, None).await.unwrap();
        assert!(!resolved);
    }

    #[tokio::test]
    async fn double_resolve_returns_false() {
        let store = InMemoryApprovalStore::new();
        store
            .save_pending("dup", "mod.c", &json!({}))
            .await
            .unwrap();
        let first = store.resolve("dup", true, None).await.unwrap();
        assert!(first);
        // Second resolve on same id must return false (already resolved)
        let second = store.resolve("dup", false, None).await.unwrap();
        assert!(!second);
        // Status must remain approved (not overwritten)
        let record = store.get_result("dup").await.unwrap().unwrap();
        assert_eq!(record.status, ApprovalStatus::Approved);
    }

    #[tokio::test]
    async fn at_max_records_eviction_happens() {
        let store = InMemoryApprovalStore::with_options(
            Duration::from_secs(60),
            Duration::from_secs(3600),
            2, // max 2 records
        );
        store
            .save_pending("first", "mod.a", &json!({}))
            .await
            .unwrap();
        store
            .save_pending("second", "mod.b", &json!({}))
            .await
            .unwrap();
        // Third insert triggers eviction of one pending record
        store
            .save_pending("third", "mod.c", &json!({}))
            .await
            .unwrap();
        // After eviction, at most 2 records should be stored.
        // third must always be present (just inserted)
        let third = store.get_result("third").await.unwrap();
        assert!(third.is_some(), "third must be present after insert");
        // Exactly one of first/second was evicted
        let first = store.get_result("first").await.unwrap();
        let second = store.get_result("second").await.unwrap();
        let remaining = [first.is_some(), second.is_some()]
            .iter()
            .filter(|&&x| x)
            .count();
        assert_eq!(remaining, 1, "one of first/second must have been evicted");
    }

    #[tokio::test]
    async fn wait_for_resolution_unblocks_after_resolve() {
        let store = Arc::new(InMemoryApprovalStore::new());
        store
            .save_pending("wait-id", "mod.w", &json!({}))
            .await
            .unwrap();

        let store_clone = Arc::clone(&store);
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(50)).await;
            store_clone.resolve("wait-id", true, None).await.unwrap();
        });

        let result = store
            .wait_for_resolution("wait-id", Duration::from_secs(5))
            .await;
        assert!(result.is_some());
        assert_eq!(result.unwrap().status, ApprovalStatus::Approved);
    }
}
