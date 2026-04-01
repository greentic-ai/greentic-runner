use std::sync::atomic::{AtomicU64, Ordering};

#[derive(Debug)]
pub struct OperatorMetrics {
    pub resolve_attempts: AtomicU64,
    pub resolve_errors: AtomicU64,
    pub invoke_attempts: AtomicU64,
    pub invoke_errors: AtomicU64,
    pub cbor_decode_errors: AtomicU64,
}

#[derive(Clone, Debug)]
pub struct OperatorMetricsSnapshot {
    pub resolve_attempts: u64,
    pub resolve_errors: u64,
    pub invoke_attempts: u64,
    pub invoke_errors: u64,
    pub cbor_decode_errors: u64,
}

impl Default for OperatorMetrics {
    fn default() -> Self {
        Self {
            resolve_attempts: AtomicU64::new(0),
            resolve_errors: AtomicU64::new(0),
            invoke_attempts: AtomicU64::new(0),
            invoke_errors: AtomicU64::new(0),
            cbor_decode_errors: AtomicU64::new(0),
        }
    }
}

impl OperatorMetrics {
    pub fn snapshot(&self) -> OperatorMetricsSnapshot {
        OperatorMetricsSnapshot {
            resolve_attempts: self.resolve_attempts.load(Ordering::Relaxed),
            resolve_errors: self.resolve_errors.load(Ordering::Relaxed),
            invoke_attempts: self.invoke_attempts.load(Ordering::Relaxed),
            invoke_errors: self.invoke_errors.load(Ordering::Relaxed),
            cbor_decode_errors: self.cbor_decode_errors.load(Ordering::Relaxed),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn snapshot_reflects_counters() {
        let metrics = OperatorMetrics::default();
        metrics.resolve_attempts.fetch_add(2, Ordering::Relaxed);
        metrics.resolve_errors.fetch_add(1, Ordering::Relaxed);
        metrics.invoke_attempts.fetch_add(4, Ordering::Relaxed);
        metrics.invoke_errors.fetch_add(3, Ordering::Relaxed);
        metrics.cbor_decode_errors.fetch_add(5, Ordering::Relaxed);

        let snapshot = metrics.snapshot();
        assert_eq!(snapshot.resolve_attempts, 2);
        assert_eq!(snapshot.resolve_errors, 1);
        assert_eq!(snapshot.invoke_attempts, 4);
        assert_eq!(snapshot.invoke_errors, 3);
        assert_eq!(snapshot.cbor_decode_errors, 5);
    }
}
