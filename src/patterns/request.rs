//! Request/response benchmark pattern.
//!
//! High-level runner with worker management, rate control, and metrics.

use crate::metrics::errors::ErrorCounter;
use crate::metrics::StatsSnapshot;
use crate::patterns::work::{BenchmarkResults, BenchmarkSummary, BenchmarkWork, WorkResult};
use crate::rate::{RateController, SharedRateController};
use crate::Stats;
use std::io::Write;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

/// Rate mode for benchmark.
#[derive(Debug, Clone, Copy)]
enum RateMode {
    /// Total rate shared across all workers.
    Total(f64),
    /// Rate per individual worker.
    PerWorker(f64),
}

impl RateMode {
    fn is_unlimited(&self) -> bool {
        match self {
            RateMode::Total(r) | RateMode::PerWorker(r) => *r <= 0.0,
        }
    }

    fn total_rate(&self, workers: usize) -> f64 {
        match self {
            RateMode::Total(r) => *r,
            RateMode::PerWorker(r) => *r * workers as f64,
        }
    }
}

/// Benchmark builder for the request/response pattern.
///
/// ```ignore
/// Benchmark::new()
///     .rate(1000.0)      // 1000 total req/s (shared pool)
///     .workers(4)
///     .duration_secs(10)
///     .work(HttpWork { client, url })
///     .run()
///     .await
///     .print_summary();
/// ```
pub struct Benchmark<W = ()> {
    rate_mode: RateMode,
    workers: usize,
    duration: Duration,
    warmup: Duration,
    snapshot_interval: Duration,
    work: Option<W>,
    csv_path: Option<PathBuf>,
    show_progress: bool,
    progress_fn: Box<dyn Fn(&StatsSnapshot) -> Option<String> + Send + 'static>,
}

impl Default for Benchmark<()> {
    fn default() -> Self {
        Self::new()
    }
}

impl Benchmark<()> {
    /// Create a new benchmark with default settings.
    ///
    /// Defaults: unlimited rate, 1 worker, 10s duration, 100ms warmup.
    pub fn new() -> Self {
        Self {
            rate_mode: RateMode::Total(0.0),
            workers: 1,
            duration: Duration::from_secs(10),
            warmup: Duration::from_millis(100),
            snapshot_interval: Duration::from_secs(1),
            work: None,
            csv_path: None,
            show_progress: true,
            progress_fn: Box::new(|snap| {
                let p50_ms = snap.latency_ns_p50 as f64 / 1_000_000.0;
                let ok_rate = if snap.sent_count > 0 {
                    snap.received_count as f64 / snap.sent_count as f64 * 100.0
                } else {
                    0.0
                };
                Some(format!(
                    "  {} req | {:.0} req/s | p50={:.2}ms | {:.0}% ok",
                    snap.received_count,
                    snap.interval_throughput(),
                    p50_ms,
                    ok_rate,
                ))
            }),
        }
    }
}

impl<W> Benchmark<W> {
    /// Set total requests per second, shared across all workers.
    ///
    /// Use `<= 0` for unlimited rate.
    pub fn rate(mut self, rate: f64) -> Self {
        self.rate_mode = RateMode::Total(rate);
        self
    }

    /// Set requests per second per worker (each worker gets its own limiter).
    ///
    /// Use `<= 0` for unlimited rate.
    pub fn rate_per_worker(mut self, rate: f64) -> Self {
        self.rate_mode = RateMode::PerWorker(rate);
        self
    }

    /// Set number of worker tasks.
    pub fn workers(mut self, workers: usize) -> Self {
        self.workers = workers.max(1);
        self
    }

    /// Set benchmark duration.
    pub fn duration(mut self, duration: Duration) -> Self {
        self.duration = duration;
        self
    }

    /// Set benchmark duration in seconds.
    pub fn duration_secs(mut self, secs: u64) -> Self {
        self.duration = Duration::from_secs(secs);
        self
    }

    /// Set warmup duration before measurements start.
    pub fn warmup(mut self, warmup: Duration) -> Self {
        self.warmup = warmup;
        self
    }

    /// Disable warmup.
    pub fn no_warmup(mut self) -> Self {
        self.warmup = Duration::ZERO;
        self
    }

    /// Set snapshot interval.
    pub fn snapshot_interval(mut self, interval: Duration) -> Self {
        self.snapshot_interval = interval;
        self
    }

    /// Set snapshot interval in seconds.
    pub fn snapshot_interval_secs(mut self, secs: u64) -> Self {
        self.snapshot_interval = Duration::from_secs(secs);
        self
    }

    /// Set CSV output file path.
    pub fn csv<P: Into<PathBuf>>(mut self, path: P) -> Self {
        self.csv_path = Some(path.into());
        self
    }

    /// Enable/disable live progress display (default: `true`).
    ///
    /// When disabled, raw CSV rows are written to stdout instead.
    pub fn progress(mut self, show: bool) -> Self {
        self.show_progress = show;
        self
    }

    /// Set a custom progress formatter.
    ///
    /// The closure receives a [`StatsSnapshot`] and returns the message to
    /// print (without timestamp — the `[N.NNNs]` prefix is always prepended).
    /// Return `None` to suppress the line for that interval.
    ///
    /// Overrides the default `"N req | N req/s | p50=…ms | N% ok"` format.
    /// Has no effect when [`progress`](Self::progress) is set to `false`.
    pub fn on_progress<F>(mut self, f: F) -> Self
    where
        F: Fn(&StatsSnapshot) -> Option<String> + Send + 'static,
    {
        self.progress_fn = Box::new(f);
        self
    }

    /// Set the work implementation.
    ///
    /// The framework clones `work` once per worker. Put shared resources
    /// (connection pools, HTTP clients) in the struct; per-worker resources
    /// go in `Work::State` via [`BenchmarkWork::init`].
    pub fn work<W2: BenchmarkWork>(self, work: W2) -> Benchmark<W2> {
        Benchmark {
            rate_mode: self.rate_mode,
            workers: self.workers,
            duration: self.duration,
            warmup: self.warmup,
            snapshot_interval: self.snapshot_interval,
            work: Some(work),
            csv_path: self.csv_path,
            show_progress: self.show_progress,
            progress_fn: self.progress_fn,
        }
    }
}

impl<W: BenchmarkWork> Benchmark<W> {
    /// Run the benchmark and return results.
    pub async fn run(self) -> BenchmarkResults {
        let work = self.work.expect("call .work() before .run()");
        let stats = Arc::new(Stats::new());
        let errors = ErrorCounter::new();
        let running = Arc::new(AtomicBool::new(true));

        let is_unlimited = self.rate_mode.is_unlimited();
        let total_rate = self.rate_mode.total_rate(self.workers);

        if is_unlimited {
            tracing::info!(
                "Benchmark: {} workers, UNLIMITED rate, {}s duration",
                self.workers,
                self.duration.as_secs()
            );
        } else {
            let mode = match self.rate_mode {
                RateMode::Total(_) => "shared",
                RateMode::PerWorker(_) => "per-worker",
            };
            tracing::info!(
                "Benchmark: {} workers, {:.1} req/s ({}), {}s duration",
                self.workers,
                total_rate,
                mode,
                self.duration.as_secs()
            );
        }

        if !self.warmup.is_zero() {
            tracing::debug!("Warmup: {:?}", self.warmup);
            tokio::time::sleep(self.warmup).await;
        }

        let mut handles = Vec::with_capacity(self.workers);

        match self.rate_mode {
            RateMode::Total(rate) => {
                let rate_ctrl = Arc::new(SharedRateController::new(rate));
                for _ in 0..self.workers {
                    let w = work.clone();
                    let stats = stats.clone();
                    let running = running.clone();
                    let rate = rate_ctrl.clone();
                    let errors = errors.clone();
                    handles.push(tokio::spawn(async move {
                        let mut state = w.init().await;
                        let mut lat_buf: Vec<u64> = Vec::with_capacity(64);
                        while running.load(Ordering::Relaxed) {
                            rate.acquire().await;
                            stats.record_sent().await;
                            match w.work(&mut state).await {
                                WorkResult::Success(latency) => {
                                    lat_buf.push(latency);
                                    if lat_buf.len() >= 64 {
                                        stats.record_received_batch(&lat_buf).await;
                                        lat_buf.clear();
                                    }
                                }
                                WorkResult::Error(reason) => {
                                    tracing::debug!(reason, "work error");
                                    errors.record(&reason).await;
                                    stats.record_error().await;
                                }
                            }
                        }
                        if !lat_buf.is_empty() {
                            stats.record_received_batch(&lat_buf).await;
                        }
                        w.cleanup(state).await;
                    }));
                }
            }
            RateMode::PerWorker(rate) => {
                for _ in 0..self.workers {
                    let w = work.clone();
                    let stats = stats.clone();
                    let running = running.clone();
                    let errors = errors.clone();
                    handles.push(tokio::spawn(async move {
                        let mut state = w.init().await;
                        let mut rate_ctrl = RateController::new(rate);
                        let mut lat_buf: Vec<u64> = Vec::with_capacity(64);
                        while running.load(Ordering::Relaxed) {
                            rate_ctrl.wait_for_next().await;
                            stats.record_sent().await;
                            match w.work(&mut state).await {
                                WorkResult::Success(latency) => {
                                    lat_buf.push(latency);
                                    if lat_buf.len() >= 64 {
                                        stats.record_received_batch(&lat_buf).await;
                                        lat_buf.clear();
                                    }
                                }
                                WorkResult::Error(reason) => {
                                    tracing::debug!(reason, "work error");
                                    errors.record(&reason).await;
                                    stats.record_error().await;
                                }
                            }
                        }
                        if !lat_buf.is_empty() {
                            stats.record_received_batch(&lat_buf).await;
                        }
                        w.cleanup(state).await;
                    }));
                }
            }
        }

        // Snapshot task
        let snap_stats = stats.clone();
        let snap_running = running.clone();
        let snapshot_interval = self.snapshot_interval;
        let csv_path = self.csv_path.clone();
        let show_progress = self.show_progress;
        let progress_fn = self.progress_fn;
        let snapshot_handle = tokio::spawn(async move {
            let mut ticker = tokio::time::interval(snapshot_interval);
            let mut csv_writer = csv_path.as_ref().map(|p| {
                let f = std::fs::File::create(p).expect("Failed to create CSV file");
                std::io::BufWriter::new(f)
            });

            if let Some(w) = csv_writer.as_mut() {
                writeln!(w, "{}", StatsSnapshot::csv_header()).ok();
            }
            if !show_progress {
                println!("{}", StatsSnapshot::csv_header());
            }

            while snap_running.load(Ordering::Relaxed) {
                ticker.tick().await;
                let snapshot = snap_stats.snapshot().await;
                if let Some(w) = csv_writer.as_mut() {
                    writeln!(w, "{}", snapshot.to_csv_row()).ok();
                }
                if show_progress {
                    if let Some(line) = progress_fn(&snapshot) {
                        crate::tprintln!("{}", line);
                    }
                } else {
                    println!("{}", snapshot.to_csv_row());
                }
            }
        });

        // Run for the configured duration then signal shutdown
        tokio::time::sleep(self.duration).await;
        running.store(false, Ordering::SeqCst);
        tracing::info!("Shutting down...");

        for h in handles {
            let _ = h.await;
        }
        let _ = snapshot_handle.await;

        let snap = stats.snapshot().await;
        let error_counts = errors.take().await;
        let ns = |v: u64| v as f64 / 1_000_000.0;

        let summary = BenchmarkSummary {
            total_ops: snap.sent_count,
            successes: snap.received_count,
            errors: snap.error_count,
            throughput: snap.total_throughput(),
            latency_p25_ms: ns(snap.latency_ns_p25),
            latency_p50_ms: ns(snap.latency_ns_p50),
            latency_p75_ms: ns(snap.latency_ns_p75),
            latency_p95_ms: ns(snap.latency_ns_p95),
            latency_p99_ms: ns(snap.latency_ns_p99),
            latency_min_ms: ns(snap.latency_ns_min),
            latency_max_ms: ns(snap.latency_ns_max),
            latency_mean_ms: snap.latency_ns_mean / 1_000_000.0,
            latency_stddev_ms: snap.latency_ns_stddev / 1_000_000.0,
            latency_samples: snap.latency_sample_count,
            error_breakdown: error_counts,
        };

        BenchmarkResults {
            summary,
            target_rate: total_rate,
            workers: self.workers,
            duration: self.duration,
            csv_path: self.csv_path,
        }
    }
}
