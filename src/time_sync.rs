//! Fast time synchronization utilities.
//!
//! Provides efficient UNIX nanosecond timestamp estimates using a cached
//! wall-clock base plus a monotonic clock delta.
//!
//! Tradeoff: after initialization, the estimate does not track later
//! wall-clock adjustments such as NTP corrections or manual clock changes.

use std::sync::OnceLock;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

struct Base {
    instant: Instant,
    unix_ns: u128,
}

fn base() -> &'static Base {
    static BASE: OnceLock<Base> = OnceLock::new();
    BASE.get_or_init(|| {
        let now_sys = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or(Duration::from_secs(0))
            .as_nanos();
        Base {
            instant: Instant::now(),
            unix_ns: now_sys,
        }
    })
}

/// Fast estimate of current UNIX time in nanoseconds.
///
/// Uses a cached `SystemTime` base timestamp and a fresh monotonic clock
/// delta on each call. This avoids repeated wall-clock to UNIX-epoch
/// conversions on hot paths while still requiring a monotonic clock read.
/// Suitable for latency measurement where low overhead matters more than
/// exact wall-clock accuracy.
///
/// # Performance
///
/// Typically somewhat faster than calling `SystemTime::now()` and converting
/// it to UNIX nanoseconds on every sample, because only the monotonic clock
/// is read after initialization.
///
/// # Tradeoffs
///
/// The returned timestamp is an estimate anchored to the wall clock at
/// initialization time. It will not reflect later wall-clock adjustments,
/// so it is best suited for short-lived latency measurements inside a single
/// process.
///
/// # Example
///
/// ```
/// use lightbench::time_sync::now_unix_ns_estimate;
///
/// let start = now_unix_ns_estimate();
/// // ... do work ...
/// let end = now_unix_ns_estimate();
/// let elapsed_ns = end - start;
/// ```
#[inline]
pub fn now_unix_ns_estimate() -> u64 {
    let b = base();
    let delta = b.instant.elapsed().as_nanos();
    let ns = b.unix_ns.saturating_add(delta);
    // Truncates on overflow, which would only occur far in the future.
    ns as u64
}

/// Calculate latency from a timestamp to now.
///
/// # Arguments
///
/// * `sent_timestamp_ns` - Unix timestamp in nanoseconds when message was sent
///
/// # Returns
///
/// Latency in nanoseconds. Returns 0 if clock skew causes negative latency.
#[inline]
pub fn latency_ns(sent_timestamp_ns: u64) -> u64 {
    now_unix_ns_estimate().saturating_sub(sent_timestamp_ns)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn monotonic_increase() {
        let a = now_unix_ns_estimate();
        let b = now_unix_ns_estimate();
        assert!(b >= a);
    }

    #[test]
    fn reasonable_timestamp() {
        let ts = now_unix_ns_estimate();
        // Should be after year 2020 (in nanoseconds)
        let year_2020_ns: u64 = 1577836800_000_000_000;
        assert!(ts > year_2020_ns);
    }

    #[test]
    fn latency_calculation() {
        let start = now_unix_ns_estimate();
        std::thread::sleep(std::time::Duration::from_millis(10));
        let lat = latency_ns(start);
        // Should be at least 10ms
        assert!(lat >= 10_000_000);
    }
}
