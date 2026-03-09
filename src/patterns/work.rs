//! Worker lifecycle traits and unified summary types.
//!
//! The framework clones a `*Work` implementor once per worker task.
//! Shared resources (`reqwest::Client`, `Arc<Pool>`, etc.) live in
//! `Self` and remain shared across workers through `Clone`. Per-worker
//! resources (dedicated connections, local buffers) live in
//! `Self::State` and are created by [`init`] / dropped by [`cleanup`].
//!
//! [`init`]: BenchmarkWork::init
//! [`cleanup`]: BenchmarkWork::cleanup

use crate::metrics::errors::ErrorCounter;
use std::collections::HashMap;
use std::future::Future;
use std::path::PathBuf;
use std::time::Duration;

// ============================================================================
// Work result types
// ============================================================================

/// Result of one request/response work unit.
#[derive(Debug, Clone)]
pub enum WorkResult {
    /// Operation succeeded with latency in nanoseconds.
    Success(u64),
    /// Operation failed with a reason string.
    Error(String),
}

impl WorkResult {
    /// Create a success result with latency in nanoseconds.
    #[inline]
    pub fn success(latency_ns: u64) -> Self {
        Self::Success(latency_ns)
    }

    /// Create an error result with a reason string.
    #[inline]
    pub fn error(reason: impl Into<String>) -> Self {
        Self::Error(reason.into())
    }
}

/// Result of polling an async task.
#[derive(Debug, Clone)]
pub enum PollResult {
    /// Task completed with end-to-end latency in nanoseconds.
    Completed { latency_ns: u64 },
    /// Task is still processing; the poller will retry.
    Pending,
    /// Poll failed with a reason string.
    Error(String),
}

// ============================================================================
// Worker lifecycle traits
// ============================================================================

/// Worker lifecycle for the **request/response** benchmark pattern.
///
/// The framework:
/// 1. **Clones** `self` once per worker task — `Arc`-backed types
///    (`reqwest::Client`, `Arc<Pool>`, etc.) remain shared across workers.
/// 2. Calls [`init`] once per worker to create per-worker state.
/// 3. Calls [`work`] in a loop (after rate limiting).
/// 4. Calls [`cleanup`] once when the benchmark ends.
///
/// # Minimal example
///
/// ```ignore
/// #[derive(Clone)]
/// struct HttpWork {
///     client: reqwest::Client,  // Arc-backed — shared across all workers
///     url: String,
/// }
///
/// impl BenchmarkWork for HttpWork {
///     type State = ();  // no per-worker state needed
///
///     async fn init(&self) -> () {}
///
///     async fn work(&self, _: &mut ()) -> WorkResult {
///         let start = now_unix_ns_estimate();
///         match self.client.get(&self.url).send().await {
///             Ok(r) if r.status().is_success() =>
///                 WorkResult::success(now_unix_ns_estimate() - start),
///             Ok(r)  => WorkResult::error(format!("HTTP {}", r.status())),
///             Err(e) => WorkResult::error(e.to_string()),
///         }
///     }
/// }
///
/// // One line, no closures, no manual cloning:
/// Benchmark::new().rate(1000.0).workers(4).work(HttpWork { client, url }).run().await
/// ```
///
/// # Per-worker state example
///
/// ```ignore
/// #[derive(Clone)]
/// struct DbWork { pool: Arc<Pool> }
///
/// impl BenchmarkWork for DbWork {
///     type State = PooledConn;  // one checked-out connection per worker
///
///     async fn init(&self) -> PooledConn {
///         self.pool.acquire().await.unwrap()  // once per worker
///     }
///
///     async fn work(&self, conn: &mut PooledConn) -> WorkResult {
///         // reuse the same connection every iteration — no checkout overhead
///         todo!()
///     }
///
///     async fn cleanup(&self, conn: PooledConn) {
///         conn.close().await;
///     }
/// }
/// ```
///
/// [`init`]: BenchmarkWork::init
/// [`work`]: BenchmarkWork::work
/// [`cleanup`]: BenchmarkWork::cleanup
pub trait BenchmarkWork: Clone + Send + Sync + 'static {
    /// Per-worker state created by [`init`] and passed to every [`work`] call.
    ///
    /// Use `()` when no per-worker state is needed.
    ///
    /// [`init`]: BenchmarkWork::init
    /// [`work`]: BenchmarkWork::work
    type State: Send + 'static;

    /// Create per-worker state. Called once before the work loop starts.
    fn init(&self) -> impl Future<Output = Self::State> + Send;

    /// Execute one unit of work. Called in a loop after rate limiting.
    fn work(&self, state: &mut Self::State) -> impl Future<Output = WorkResult> + Send;

    /// Clean up per-worker state when the benchmark ends.
    ///
    /// Default: drops the state.
    fn cleanup(&self, _state: Self::State) -> impl Future<Output = ()> + Send {
        async {}
    }
}

/// Worker lifecycle for the **producer** side of a producer/consumer benchmark.
pub trait ProducerWork: Clone + Send + Sync + 'static {
    /// Per-worker state.
    type State: Send + 'static;

    fn init(&self) -> impl Future<Output = Self::State> + Send;

    /// Produce one item at the controlled rate.
    ///
    /// Return `Ok(())` on success or `Err(reason)` on failure.
    fn produce(&self, state: &mut Self::State) -> impl Future<Output = Result<(), String>> + Send;

    fn cleanup(&self, _state: Self::State) -> impl Future<Output = ()> + Send {
        async {}
    }
}

/// Worker lifecycle for the **consumer** side of a producer/consumer benchmark.
pub trait ConsumerWork: Clone + Send + Sync + 'static {
    /// Per-worker state.
    type State: Send + 'static;

    fn init(&self) -> impl Future<Output = Self::State> + Send;

    /// Try to consume one item.
    ///
    /// Return `Some(latency_ns)` when an item was consumed (latency = time
    /// the item spent in the queue). Return `None` when the queue is empty;
    /// the worker yields and retries immediately.
    fn consume(&self, state: &mut Self::State) -> impl Future<Output = Option<u64>> + Send;

    fn cleanup(&self, _state: Self::State) -> impl Future<Output = ()> + Send {
        async {}
    }
}

/// Worker lifecycle for the **submit** side of an async-task benchmark.
pub trait SubmitWork: Clone + Send + Sync + 'static {
    /// Per-worker state.
    type State: Send + 'static;

    fn init(&self) -> impl Future<Output = Self::State> + Send;

    /// Submit one task at the controlled rate.
    ///
    /// Return `Some(task_id)` on success or `None` on failure.
    fn submit(&self, state: &mut Self::State) -> impl Future<Output = Option<u64>> + Send;

    fn cleanup(&self, _state: Self::State) -> impl Future<Output = ()> + Send {
        async {}
    }
}

/// Worker lifecycle for the **poll** side of an async-task benchmark.
pub trait PollWork: Clone + Send + Sync + 'static {
    /// Per-worker state.
    type State: Send + 'static;

    fn init(&self) -> impl Future<Output = Self::State> + Send;

    /// Check a task for completion.
    fn poll(
        &self,
        task_id: u64,
        state: &mut Self::State,
    ) -> impl Future<Output = PollResult> + Send;

    fn cleanup(&self, _state: Self::State) -> impl Future<Output = ()> + Send {
        async {}
    }
}

// ============================================================================
// Consistent summary types
// ============================================================================

/// Final metrics from one role in a benchmark run.
///
/// All latency fields are in **milliseconds**. Fields are `0.0` / `0` for
/// roles that do not measure latency (e.g. producers, submitters).
#[derive(Debug, Clone)]
pub struct BenchmarkSummary {
    /// Total operations attempted.
    pub total_ops: u64,
    /// Operations that succeeded.
    pub successes: u64,
    /// Operations that failed.
    pub errors: u64,
    /// Achieved throughput in operations/second.
    pub throughput: f64,
    pub latency_p25_ms: f64,
    pub latency_p50_ms: f64,
    pub latency_p75_ms: f64,
    pub latency_p95_ms: f64,
    pub latency_p99_ms: f64,
    pub latency_min_ms: f64,
    pub latency_max_ms: f64,
    pub latency_mean_ms: f64,
    pub latency_stddev_ms: f64,
    /// Number of latency samples recorded (`0` when latency is not tracked).
    pub latency_samples: u64,
    /// Error counts grouped by reason string.
    pub error_breakdown: HashMap<String, u64>,
}

impl BenchmarkSummary {
    /// Print a labelled block for this summary.
    pub fn print(&self, label: &str) {
        let success_rate = if self.total_ops > 0 {
            self.successes as f64 / self.total_ops as f64 * 100.0
        } else {
            0.0
        };
        println!("  {label}");
        println!("    Throughput:  {:.2} ops/s", self.throughput);
        println!(
            "    Operations:  {} total, {} ok, {} errors ({:.1}% success)",
            self.total_ops, self.successes, self.errors, success_rate
        );
        if !self.error_breakdown.is_empty() {
            ErrorCounter::print_summary(&self.error_breakdown);
        }
        if self.latency_samples > 0 {
            println!("    Latency (ms):");
            println!(
                "      min={:.3}  p50={:.3}  p95={:.3}  p99={:.3}  max={:.3}",
                self.latency_min_ms,
                self.latency_p50_ms,
                self.latency_p95_ms,
                self.latency_p99_ms,
                self.latency_max_ms,
            );
            println!(
                "      mean={:.3}  stddev={:.3}",
                self.latency_mean_ms, self.latency_stddev_ms
            );
        }
    }

    /// Success rate as a percentage (0.0–100.0).
    pub fn success_rate(&self) -> f64 {
        if self.total_ops > 0 {
            self.successes as f64 / self.total_ops as f64 * 100.0
        } else {
            0.0
        }
    }

    pub fn p50_latency_ms(&self) -> f64 {
        self.latency_p50_ms
    }
    pub fn p95_latency_ms(&self) -> f64 {
        self.latency_p95_ms
    }
    pub fn p99_latency_ms(&self) -> f64 {
        self.latency_p99_ms
    }
}

// ============================================================================
// Result types returned by each pattern
// ============================================================================

/// Results from a [`super::request::Benchmark`] run.
#[derive(Debug)]
pub struct BenchmarkResults {
    pub summary: BenchmarkSummary,
    /// Target total rate (req/s); `0` means unlimited.
    pub target_rate: f64,
    pub workers: usize,
    pub duration: Duration,
    pub csv_path: Option<PathBuf>,
}

impl BenchmarkResults {
    pub fn print_summary(&self) {
        let rate_str = if self.target_rate <= 0.0 {
            "unlimited".to_string()
        } else {
            format!("{:.0}", self.target_rate)
        };
        println!();
        println!("BENCHMARK RESULTS");
        println!(
            "  Duration: {:.1}s | Workers: {} | Target: {} req/s",
            self.duration.as_secs_f64(),
            self.workers,
            rate_str,
        );
        println!();
        self.summary.print("Request");
        if let Some(p) = &self.csv_path {
            println!();
            println!("  CSV: {}", p.display());
        }
        println!();
    }

    pub fn throughput(&self) -> f64 {
        self.summary.throughput
    }
    pub fn p50_latency_ms(&self) -> f64 {
        self.summary.p50_latency_ms()
    }
    pub fn p95_latency_ms(&self) -> f64 {
        self.summary.p95_latency_ms()
    }
    pub fn p99_latency_ms(&self) -> f64 {
        self.summary.p99_latency_ms()
    }
    pub fn errors(&self) -> u64 {
        self.summary.errors
    }
    pub fn sent(&self) -> u64 {
        self.summary.total_ops
    }
    pub fn received(&self) -> u64 {
        self.summary.successes
    }
}

/// Results from a [`super::producer_consumer::ProducerConsumerBenchmark`] run.
#[derive(Debug)]
pub struct ProducerConsumerResults {
    pub produced: BenchmarkSummary,
    pub consumed: BenchmarkSummary,
    pub producers: usize,
    pub consumers: usize,
    pub target_rate: f64,
    pub duration: Duration,
    pub csv_path: Option<PathBuf>,
}

impl ProducerConsumerResults {
    pub fn print_summary(&self) {
        let in_flight = self
            .produced
            .successes
            .saturating_sub(self.consumed.successes);
        println!();
        println!("PRODUCER/CONSUMER RESULTS");
        println!(
            "  Duration: {:.1}s | Producers: {} | Consumers: {} | Target: {:.0} msg/s",
            self.duration.as_secs_f64(),
            self.producers,
            self.consumers,
            self.target_rate,
        );
        println!("  In-flight: {}", in_flight);
        println!();
        self.produced.print("Produced");
        println!();
        self.consumed.print("Consumed (queue latency)");
        if let Some(p) = &self.csv_path {
            println!();
            println!("  CSV: {}", p.display());
        }
        println!();
    }
}

/// Results from an [`super::async_task::AsyncTaskBenchmark`] run.
#[derive(Debug)]
pub struct AsyncTaskResults {
    pub submitted: BenchmarkSummary,
    pub completed: BenchmarkSummary,
    pub submit_workers: usize,
    pub poll_workers: usize,
    pub target_rate: f64,
    pub duration: Duration,
    pub csv_path: Option<PathBuf>,
}

impl AsyncTaskResults {
    pub fn print_summary(&self) {
        let in_flight = self
            .submitted
            .successes
            .saturating_sub(self.completed.successes);
        println!();
        println!("ASYNC TASK RESULTS");
        println!(
            "  Duration: {:.1}s | Submit: {} workers @ {:.0}/s | Poll: {} workers",
            self.duration.as_secs_f64(),
            self.submit_workers,
            self.target_rate,
            self.poll_workers,
        );
        println!("  In-flight: {}", in_flight);
        println!();
        self.submitted.print("Submit");
        println!();
        self.completed.print("Completed (task latency)");
        if let Some(p) = &self.csv_path {
            println!();
            println!("  CSV: {}", p.display());
        }
        println!();
    }
}
