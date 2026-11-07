//! I/O statistics and telemetry for MIO.
//!
//! Tracks per-instance and global metrics for monitoring and tuning:
//! - Submission/completion rates
//! - Latency histograms (bucket-based, lock-free)
//! - Ring utilization (how full SQ/CQ typically are)
//! - Worker pool utilization
//!
//! Based on: Linux blk-mq I/O statistics, io_uring fdinfo counters.

use core::sync::atomic::{AtomicU64, Ordering};

/// Latency histogram buckets (in microseconds).
///
/// Exponentially spaced for covering sub-microsecond to multi-second ranges:
/// [0-1µs, 1-4µs, 4-16µs, 16-64µs, 64-256µs, 256µs-1ms, 1-4ms, 4-16ms,
///  16-64ms, 64-256ms, 256ms-1s, >1s]
pub const LATENCY_BUCKETS: [u64; 12] = [
    1, 4, 16, 64, 256, 1_000, 4_000, 16_000, 64_000, 256_000, 1_000_000, u64::MAX,
];

/// Number of latency buckets.
pub const NUM_BUCKETS: usize = LATENCY_BUCKETS.len();

/// Per-instance I/O statistics.
pub struct MioStats {
    /// Total SQEs submitted.
    pub submissions: AtomicU64,
    /// Total CQEs reaped.
    pub completions: AtomicU64,
    /// Total SQEs that resulted in errors.
    pub errors: AtomicU64,
    /// Total CQ overflow events (CQE dropped).
    pub cq_overflows: AtomicU64,
    /// Latency histogram: count of completions in each bucket.
    pub latency_buckets: [AtomicU64; NUM_BUCKETS],
    /// Sum of all completion latencies (for computing average).
    pub latency_sum_us: AtomicU64,
    /// Minimum observed latency in microseconds.
    pub latency_min_us: AtomicU64,
    /// Maximum observed latency in microseconds.
    pub latency_max_us: AtomicU64,
    /// Peak SQ utilization (max entries in SQ at any point).
    pub sq_peak_utilization: AtomicU64,
    /// Peak CQ utilization.
    pub cq_peak_utilization: AtomicU64,
    /// Total bytes read.
    pub bytes_read: AtomicU64,
    /// Total bytes written.
    pub bytes_written: AtomicU64,
}

impl MioStats {
    /// Create zeroed stats.
    pub fn new() -> Self {
        MioStats {
            submissions: AtomicU64::new(0),
            completions: AtomicU64::new(0),
            errors: AtomicU64::new(0),
            cq_overflows: AtomicU64::new(0),
            latency_buckets: core::array::from_fn(|_| AtomicU64::new(0)),
            latency_sum_us: AtomicU64::new(0),
            latency_min_us: AtomicU64::new(u64::MAX),
            latency_max_us: AtomicU64::new(0),
            sq_peak_utilization: AtomicU64::new(0),
            cq_peak_utilization: AtomicU64::new(0),
            bytes_read: AtomicU64::new(0),
            bytes_written: AtomicU64::new(0),
        }
    }

    /// Record a completed I/O operation with its latency.
    pub fn record_completion(&self, latency_us: u64, bytes: u64, is_write: bool) {
        self.completions.fetch_add(1, Ordering::Relaxed);

        // Update latency histogram.
        let bucket = self.latency_bucket(latency_us);
        self.latency_buckets[bucket].fetch_add(1, Ordering::Relaxed);
        self.latency_sum_us.fetch_add(latency_us, Ordering::Relaxed);

        // Update min/max (relaxed CAS loop — approximate is fine for stats).
        let _ = self.latency_min_us.fetch_update(Ordering::Relaxed, Ordering::Relaxed, |cur| {
            if latency_us < cur { Some(latency_us) } else { None }
        });
        let _ = self.latency_max_us.fetch_update(Ordering::Relaxed, Ordering::Relaxed, |cur| {
            if latency_us > cur { Some(latency_us) } else { None }
        });

        // Update byte counters.
        if is_write {
            self.bytes_written.fetch_add(bytes, Ordering::Relaxed);
        } else {
            self.bytes_read.fetch_add(bytes, Ordering::Relaxed);
        }
    }

    /// Record an error.
    pub fn record_error(&self) {
        self.errors.fetch_add(1, Ordering::Relaxed);
    }

    /// Record a CQ overflow event.
    pub fn record_overflow(&self) {
        self.cq_overflows.fetch_add(1, Ordering::Relaxed);
    }

    /// Update peak SQ utilization.
    pub fn update_sq_peak(&self, current: u64) {
        let _ = self.sq_peak_utilization.fetch_update(Ordering::Relaxed, Ordering::Relaxed, |cur| {
            if current > cur { Some(current) } else { None }
        });
    }

    /// Update peak CQ utilization.
    pub fn update_cq_peak(&self, current: u64) {
        let _ = self.cq_peak_utilization.fetch_update(Ordering::Relaxed, Ordering::Relaxed, |cur| {
            if current > cur { Some(current) } else { None }
        });
    }

    /// Find the histogram bucket for a latency value.
    fn latency_bucket(&self, latency_us: u64) -> usize {
        for (i, &threshold) in LATENCY_BUCKETS.iter().enumerate() {
            if latency_us <= threshold {
                return i;
            }
        }
        NUM_BUCKETS - 1
    }

    /// Compute the average completion latency in microseconds.
    pub fn average_latency_us(&self) -> u64 {
        let completions = self.completions.load(Ordering::Relaxed);
        if completions == 0 {
            return 0;
        }
        self.latency_sum_us.load(Ordering::Relaxed) / completions
    }

    /// Get a snapshot of all stats (for telemetry export).
    pub fn snapshot(&self) -> StatsSnapshot {
        StatsSnapshot {
            submissions: self.submissions.load(Ordering::Relaxed),
            completions: self.completions.load(Ordering::Relaxed),
            errors: self.errors.load(Ordering::Relaxed),
            cq_overflows: self.cq_overflows.load(Ordering::Relaxed),
            avg_latency_us: self.average_latency_us(),
            min_latency_us: self.latency_min_us.load(Ordering::Relaxed),
            max_latency_us: self.latency_max_us.load(Ordering::Relaxed),
            sq_peak: self.sq_peak_utilization.load(Ordering::Relaxed),
            cq_peak: self.cq_peak_utilization.load(Ordering::Relaxed),
            bytes_read: self.bytes_read.load(Ordering::Relaxed),
            bytes_written: self.bytes_written.load(Ordering::Relaxed),
        }
    }
}

/// Immutable snapshot of statistics at a point in time.
#[derive(Debug, Clone)]
pub struct StatsSnapshot {
    pub submissions: u64,
    pub completions: u64,
    pub errors: u64,
    pub cq_overflows: u64,
    pub avg_latency_us: u64,
    pub min_latency_us: u64,
    pub max_latency_us: u64,
    pub sq_peak: u64,
    pub cq_peak: u64,
    pub bytes_read: u64,
    pub bytes_written: u64,
}
