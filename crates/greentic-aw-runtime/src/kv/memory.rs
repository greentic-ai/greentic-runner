//! In-process [`AwKv`] backed by a single mutex + per-key expiry timestamp.
//! Always compiled (no external dependency). Ephemeral: all data is lost on
//! process exit.

use std::collections::HashMap;
use std::sync::Mutex;
use std::time::{Duration, Instant};

use super::{AwKv, KvFut, encode_u64};

struct Entry {
    bytes: Vec<u8>,
    expires_at: Instant,
}

/// Ephemeral in-process key-value store.
#[derive(Default)]
pub struct MemoryKv {
    map: Mutex<HashMap<String, Entry>>,
}

impl MemoryKv {
    /// Create an empty store.
    pub fn new() -> Self {
        Self::default()
    }

    /// Lock the map, evicting the touched key first if it has expired. Returns
    /// a `StateError::Redis` on mutex poisoning (kept as `Redis` so the loop's
    /// existing error handling is unchanged).
    fn with_map<R>(
        &self,
        f: impl FnOnce(&mut HashMap<String, Entry>, Instant) -> R,
    ) -> Result<R, crate::error::StateError> {
        let now = Instant::now();
        let mut guard = self
            .map
            .lock()
            .map_err(|e| crate::error::StateError::Redis(format!("memory kv poisoned: {e}")))?;
        // Opportunistic sweep of expired entries keeps the map bounded.
        guard.retain(|_, entry| entry.expires_at > now);
        Ok(f(&mut guard, now))
    }
}

impl AwKv for MemoryKv {
    fn get<'a>(&'a self, key: &'a str) -> KvFut<'a, Option<Vec<u8>>> {
        Box::pin(async move { self.with_map(|map, _now| map.get(key).map(|e| e.bytes.clone())) })
    }

    fn set_ex<'a>(&'a self, key: &'a str, val: Vec<u8>, ttl: Duration) -> KvFut<'a, ()> {
        Box::pin(async move {
            self.with_map(|map, now| {
                map.insert(
                    key.to_string(),
                    Entry {
                        bytes: val,
                        expires_at: now + ttl,
                    },
                );
            })
        })
    }

    fn del<'a>(&'a self, key: &'a str) -> KvFut<'a, ()> {
        Box::pin(async move {
            self.with_map(|map, _now| {
                map.remove(key);
            })
        })
    }

    fn set_nx<'a>(&'a self, key: &'a str, val: Vec<u8>, ttl: Duration) -> KvFut<'a, bool> {
        Box::pin(async move {
            self.with_map(|map, now| {
                if map.contains_key(key) {
                    false
                } else {
                    map.insert(
                        key.to_string(),
                        Entry {
                            bytes: val,
                            expires_at: now + ttl,
                        },
                    );
                    true
                }
            })
        })
    }

    fn compare_refresh<'a>(
        &'a self,
        key: &'a str,
        expected: &'a [u8],
        ttl: Duration,
    ) -> KvFut<'a, bool> {
        Box::pin(async move {
            self.with_map(|map, now| match map.get_mut(key) {
                Some(entry) if entry.bytes == expected => {
                    entry.expires_at = now + ttl;
                    true
                }
                _ => false,
            })
        })
    }

    fn compare_del<'a>(&'a self, key: &'a str, expected: &'a [u8]) -> KvFut<'a, bool> {
        Box::pin(async move {
            self.with_map(|map, _now| match map.get(key) {
                Some(entry) if entry.bytes == expected => {
                    map.remove(key);
                    true
                }
                _ => false,
            })
        })
    }

    fn incr_by<'a>(&'a self, key: &'a str, delta: u64, ttl: Duration) -> KvFut<'a, u64> {
        Box::pin(async move {
            self.with_map(|map, now| {
                let current = map
                    .get(key)
                    .and_then(|e| e.bytes.as_slice().try_into().ok())
                    .map(u64::from_be_bytes)
                    .unwrap_or(0);
                let next = current.saturating_add(delta);
                map.insert(
                    key.to_string(),
                    Entry {
                        bytes: encode_u64(next),
                        expires_at: now + ttl,
                    },
                );
                next
            })
        })
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;
    use std::time::Duration;

    #[tokio::test]
    async fn set_get_del_roundtrip() {
        let kv = MemoryKv::new();
        assert_eq!(kv.get("k").await.unwrap(), None);
        kv.set_ex("k", b"v".to_vec(), Duration::from_secs(60))
            .await
            .unwrap();
        assert_eq!(kv.get("k").await.unwrap(), Some(b"v".to_vec()));
        kv.del("k").await.unwrap();
        assert_eq!(kv.get("k").await.unwrap(), None);
    }

    #[tokio::test]
    async fn expired_key_reads_none() {
        let kv = MemoryKv::new();
        kv.set_ex("k", b"v".to_vec(), Duration::from_millis(1))
            .await
            .unwrap();
        tokio::time::sleep(Duration::from_millis(10)).await;
        assert_eq!(kv.get("k").await.unwrap(), None);
    }

    #[tokio::test]
    async fn set_nx_only_first_wins() {
        let kv = MemoryKv::new();
        assert!(
            kv.set_nx("lock", b"a".to_vec(), Duration::from_secs(60))
                .await
                .unwrap()
        );
        assert!(
            !kv.set_nx("lock", b"b".to_vec(), Duration::from_secs(60))
                .await
                .unwrap()
        );
        assert_eq!(kv.get("lock").await.unwrap(), Some(b"a".to_vec()));
    }

    #[tokio::test]
    async fn compare_del_and_refresh_check_ownership() {
        let kv = MemoryKv::new();
        kv.set_nx("lock", b"owner".to_vec(), Duration::from_secs(60))
            .await
            .unwrap();
        // Wrong token: no-op.
        assert!(
            !kv.compare_refresh("lock", b"other", Duration::from_secs(60))
                .await
                .unwrap()
        );
        assert!(!kv.compare_del("lock", b"other").await.unwrap());
        assert_eq!(kv.get("lock").await.unwrap(), Some(b"owner".to_vec()));
        // Right token: succeeds.
        assert!(
            kv.compare_refresh("lock", b"owner", Duration::from_secs(60))
                .await
                .unwrap()
        );
        assert!(kv.compare_del("lock", b"owner").await.unwrap());
        assert_eq!(kv.get("lock").await.unwrap(), None);
    }

    #[tokio::test]
    async fn incr_by_accumulates_and_get_u64_reads() {
        let kv = MemoryKv::new();
        assert_eq!(kv.get_u64("c").await.unwrap(), 0);
        assert_eq!(
            kv.incr_by("c", 5, Duration::from_secs(60)).await.unwrap(),
            5
        );
        assert_eq!(
            kv.incr_by("c", 3, Duration::from_secs(60)).await.unwrap(),
            8
        );
        assert_eq!(kv.get_u64("c").await.unwrap(), 8);
    }
}
