//! Rate limiting with token bucket algorithm.
//!
//! Provides open-loop rate control for message sending without external feedback.
//! Uses adaptive ticking to amortize timer overhead at high rates.
//!
//! Two controllers are available:
//! - [`RateController`]: Per-worker rate limiter (each worker gets its own)
//! - [`SharedRateController`]: Shared across workers (global rate limit)

use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;
use tokio::time::{interval, Interval, MissedTickBehavior};

/// Rate controller for open-loop message sending.
///
/// Uses a token bucket algorithm with adaptive tick rate to balance
/// accuracy and timer overhead.
///
/// # Example
///
/// ```
/// use lightbench::rate::RateController;
///
/// # tokio_test::block_on(async {
/// let mut rate = RateController::new(100.0); // 100 msg/sec
///
/// for _ in 0..10 {
///     rate.wait_for_next().await;
///     // send message...
/// }
/// # });
/// ```
pub struct RateController {
    #[allow(dead_code)]
    rate_per_sec: f64,
    ticker: Interval,
    tokens: u32,
    tokens_per_tick: u32,
    frac_per_tick: u32, // Q24.8 fixed-point fractional tokens per tick
    frac_accum: u32,    // accumulator for fractional tokens
    max_tokens: u32,
}

impl RateController {
    /// Create new rate controller for target messages per second.
    ///
    /// Implementation: token bucket with adaptive tick (≤100 ticks/s)
    /// to amortize timer cost at high rates.
    pub fn new(msgs_per_second: f64) -> Self {
        // Adaptive tick: aim for ≤100 ticks per second; not less than 10ms
        let ticks_per_sec = msgs_per_second.clamp(1.0, 100.0);
        let tick = Duration::from_secs_f64(1.0 / ticks_per_sec);
        let mut ticker = interval(tick);
        ticker.set_missed_tick_behavior(MissedTickBehavior::Burst);

        let per_tick = msgs_per_second * tick.as_secs_f64();
        let tokens_per_tick = per_tick.floor() as u32;
        // Q24.8 fixed point for fractional tokens per tick
        const FP_SCALE: f64 = 256.0; // 2^8
        let frac_per_tick = ((per_tick - per_tick.floor()) * FP_SCALE) as u32;
        // Allow short bursts up to ~10 ticks worth to smooth scheduling hiccups
        let max_tokens = (tokens_per_tick.saturating_mul(10)).max(1);

        Self {
            rate_per_sec: msgs_per_second.max(0.0),
            ticker,
            // Start with 1 token to avoid cold-start delay
            tokens: 1,
            tokens_per_tick,
            frac_per_tick,
            frac_accum: 0,
            max_tokens,
        }
    }

    /// Wait until it's time to send the next message.
    ///
    /// Returns immediately if tokens are available, otherwise waits
    /// for the next tick to refill.
    #[inline(always)]
    pub async fn wait_for_next(&mut self) {
        // Fast path: if we have tokens, consume and return immediately
        if self.tokens > 0 {
            self.tokens -= 1;
            return;
        }

        // Otherwise, refill until at least one token is available
        loop {
            self.ticker.tick().await;
            // Integer refill
            let mut new_tokens = self.tokens.saturating_add(self.tokens_per_tick);
            // Handle fractional accumulation (Q24.8)
            let acc = self.frac_accum + self.frac_per_tick;
            let carry = acc >> 8; // divide by FP scale (256)
            self.frac_accum = acc & 0xFF;
            new_tokens = new_tokens.saturating_add(carry);
            self.tokens = new_tokens.min(self.max_tokens);
            if self.tokens > 0 {
                self.tokens -= 1;
                break;
            }
        }
    }

    /// Get configured rate (messages per second).
    pub fn rate(&self) -> f64 {
        self.rate_per_sec
    }

    /// Get theoretical interval between messages.
    pub fn interval(&self) -> Duration {
        if self.rate_per_sec > 0.0 {
            Duration::from_secs_f64(1.0 / self.rate_per_sec)
        } else {
            Duration::from_secs(0)
        }
    }
}

/// Shared rate controller for multiple workers.
///
/// Unlike `RateController`, this can be shared across workers using `Arc`.
/// All workers compete for tokens from the same pool, ensuring the total
/// rate matches the target regardless of worker count.
///
/// # Example
///
/// ```
/// use lightbench::rate::SharedRateController;
/// use std::sync::Arc;
///
/// # tokio_test::block_on(async {
/// let rate = Arc::new(SharedRateController::new(1000.0)); // 1000 msg/s total
///
/// // Spawn multiple workers sharing the same rate limiter
/// for _ in 0..4 {
///     let rate = rate.clone();
///     tokio::spawn(async move {
///         for _ in 0..10 {
///             rate.acquire().await;
///             // send message...
///         }
///     });
/// }
/// # });
/// ```
pub struct SharedRateController {
    rate_per_sec: f64,
    /// Tokens available (scaled by TOKEN_SCALE for sub-token precision)
    tokens: AtomicU64,
    /// Last refill timestamp in nanoseconds
    last_refill_ns: AtomicU64,
    /// Maximum tokens (scaled)
    max_tokens: u64,
    /// Tokens added per nanosecond (scaled, fixed-point)
    tokens_per_ns: f64,
}

// Scale factor for token precision (allows fractional tokens)
const TOKEN_SCALE: u64 = 1_000_000;

impl SharedRateController {
    /// Create a new shared rate controller.
    ///
    /// # Arguments
    /// * `msgs_per_second` - Target rate in messages per second.
    ///   If <= 0, `acquire()` returns immediately (unlimited rate).
    pub fn new(msgs_per_second: f64) -> Self {
        let rate = msgs_per_second.max(0.0);
        let tokens_per_ns = rate / 1_000_000_000.0;
        // Allow burst of up to 100ms worth of tokens
        let max_tokens = ((rate * 0.1).max(1.0) * TOKEN_SCALE as f64) as u64;
        let now_ns = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos() as u64;

        Self {
            rate_per_sec: rate,
            tokens: AtomicU64::new(TOKEN_SCALE), // Start with 1 token
            last_refill_ns: AtomicU64::new(now_ns),
            max_tokens,
            tokens_per_ns,
        }
    }

    /// Check if this controller is unlimited (rate <= 0).
    #[inline]
    pub fn is_unlimited(&self) -> bool {
        self.rate_per_sec <= 0.0
    }

    /// Acquire permission to send one message.
    ///
    /// Returns immediately if rate <= 0 (unlimited mode).
    /// Otherwise waits until a token is available.
    #[inline]
    pub async fn acquire(&self) {
        if self.is_unlimited() {
            return;
        }

        loop {
            self.refill();

            // Try to atomically decrement tokens
            let current = self.tokens.load(Ordering::Relaxed);
            if current >= TOKEN_SCALE {
                // CAS to consume one token
                if self
                    .tokens
                    .compare_exchange_weak(
                        current,
                        current - TOKEN_SCALE,
                        Ordering::Relaxed,
                        Ordering::Relaxed,
                    )
                    .is_ok()
                {
                    return;
                }
                // CAS failed, retry immediately
                continue;
            }

            // No tokens available, wait a bit and retry
            // Adaptive sleep: shorter at high rates
            let sleep_us = if self.rate_per_sec > 10000.0 {
                10 // 10µs for >10k/s
            } else if self.rate_per_sec > 1000.0 {
                100 // 100µs for >1k/s
            } else {
                1000 // 1ms for lower rates
            };
            tokio::time::sleep(Duration::from_micros(sleep_us)).await;
        }
    }

    /// Refill tokens based on elapsed time.
    #[inline]
    fn refill(&self) {
        let now_ns = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos() as u64;

        let last = self.last_refill_ns.load(Ordering::Relaxed);
        let elapsed_ns = now_ns.saturating_sub(last);

        if elapsed_ns == 0 {
            return;
        }

        // Calculate new tokens
        let new_tokens = (elapsed_ns as f64 * self.tokens_per_ns * TOKEN_SCALE as f64) as u64;
        if new_tokens == 0 {
            return;
        }

        // Update last refill time (best effort, don't need perfect accuracy)
        let _ = self.last_refill_ns.compare_exchange(
            last,
            now_ns,
            Ordering::Relaxed,
            Ordering::Relaxed,
        );

        // Add tokens (capped at max)
        let _ = self
            .tokens
            .fetch_update(Ordering::Relaxed, Ordering::Relaxed, |current| {
                Some(current.saturating_add(new_tokens).min(self.max_tokens))
            });
    }

    /// Get configured rate (messages per second).
    pub fn rate(&self) -> f64 {
        self.rate_per_sec
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::time::Instant as TokioInstant;

    #[tokio::test]
    async fn rate_controller_spaces_events() {
        // 50 msg/s => ~20ms interval
        let mut rc = RateController::new(50.0);
        let expected = Duration::from_millis(20);

        let mut times: Vec<TokioInstant> = Vec::new();

        // Warm-up first token
        rc.wait_for_next().await;
        times.push(TokioInstant::now());

        // Collect 10 ticks
        for _ in 0..10 {
            rc.wait_for_next().await;
            times.push(TokioInstant::now());
        }

        // Compute total elapsed across 10 intervals
        let total_elapsed = times
            .last()
            .unwrap()
            .saturating_duration_since(*times.first().unwrap());
        let avg = total_elapsed / 10;

        // Allow generous bounds due to scheduler jitter
        let lower = expected.mul_f32(0.5); // >= 10ms
        let upper = expected.mul_f32(5.0); // <= 100ms

        assert!(
            avg >= lower,
            "avg interval too small: {:?} < {:?}",
            avg,
            lower
        );
        assert!(
            avg <= upper,
            "avg interval too large: {:?} > {:?}",
            avg,
            upper
        );
    }

    #[tokio::test]
    async fn high_rate_does_not_block() {
        let mut rc = RateController::new(10000.0);

        let start = TokioInstant::now();
        for _ in 0..100 {
            rc.wait_for_next().await;
        }
        let elapsed = start.elapsed();

        // Should complete reasonably fast (< 1s for 100 msgs at 10k/s)
        assert!(elapsed < Duration::from_secs(1));
    }
}
