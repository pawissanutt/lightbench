//! Request/response benchmark pattern.
//!
//! High-level runner with worker management, rate control, and metrics.

use crate::metrics::errors::ErrorCounter;
use crate::metrics::StatsSnapshot;
use crate::patterns::work::{BenchmarkResults, BenchmarkSummary, BenchmarkWork, WorkResult};
use crate::rate::DynamicRateController;
use crate::Stats;
use std::io::Write;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

type ProgressFn = Arc<dyn Fn(&StatsSnapshot) -> Option<String> + Send + Sync + 'static>;

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
    ramp_up: Option<Duration>,
    ramp_start_rate: f64,
    burst_factor: f64,
    show_ramp_progress: bool,
    work: Option<W>,
    csv_path: Option<PathBuf>,
    show_progress: bool,
    progress_fn: ProgressFn,
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
            ramp_up: None,
            ramp_start_rate: 0.0,
            burst_factor: 0.1,
            show_ramp_progress: true,
            work: None,
            csv_path: None,
            show_progress: true,
            progress_fn: Arc::new(|snap| {
                let p50_ms = snap.latency_ns_p50 as f64 / 1_000_000.0;
                let ok_rate = if snap.sent_count > 0 {
                    snap.received_count as f64 / snap.sent_count as f64 * 100.0
                } else {
                    0.0
                };
                Some(format!(
                    "  {} req | curr {:.0} req/s | total {:.0} req/s | p50={:.2}ms | {:.0}% ok",
                    snap.received_count,
                    snap.interval_throughput(),
                    snap.total_throughput(),
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

    /// Set the ramp-up duration before the measured benchmark starts.
    ///
    /// During ramp-up, workers run at a linearly increasing rate from
    /// [`ramp_start_rate`](Self::ramp_start_rate) up to the configured target
    /// rate.  Stats are collected into the same pool as the main phase.
    pub fn ramp_up(mut self, duration: Duration) -> Self {
        self.ramp_up = Some(duration);
        self
    }

    /// Set the initial rate at the start of the ramp-up period (default: `0.0`).
    ///
    /// Has no effect if [`ramp_up`](Self::ramp_up) is not set.
    pub fn ramp_start_rate(mut self, rate: f64) -> Self {
        self.ramp_start_rate = rate.max(0.0);
        self
    }

    /// Set the burst factor for the rate controller (default: `0.1`).
    ///
    /// Controls how many seconds' worth of tokens may accumulate while the
    /// system is idle.  For example `0.5` allows up to 500 ms of burst before
    /// the bucket is full.
    pub fn burst_factor(mut self, factor: f64) -> Self {
        self.burst_factor = factor.clamp(0.001, 1000.0);
        self
    }

    /// Show progress during the ramp-up period (default: `true`).
    ///
    /// When `true`, snapshot lines are displayed with a `[RAMP]` prefix
    /// during the ramp phase. Set to `false` to suppress output until
    /// ramp completes.
    pub fn show_ramp_progress(mut self, show: bool) -> Self {
        self.show_ramp_progress = show;
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
        F: Fn(&StatsSnapshot) -> Option<String> + Send + Sync + 'static,
    {
        self.progress_fn = Arc::new(f);
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
            ramp_up: self.ramp_up,
            ramp_start_rate: self.ramp_start_rate,
            burst_factor: self.burst_factor,
            show_ramp_progress: self.show_ramp_progress,
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
        let in_ramp = Arc::new(AtomicBool::new(self.ramp_up.is_some()));
        let total_rate = self.rate_mode.total_rate(self.workers);

        log_config(&self.rate_mode, self.workers, &self.duration);

        if !self.warmup.is_zero() {
            tracing::debug!("Warmup: {:?}", self.warmup);
            tokio::time::sleep(self.warmup).await;
        }

        let (rate_ctrls, ramp_target) = build_rate_controllers(
            &self.rate_mode,
            self.workers,
            self.ramp_up.is_some(),
            self.ramp_start_rate,
            self.burst_factor,
        );

        let handles: Vec<_> = rate_ctrls
            .iter()
            .map(|ctrl| {
                spawn_worker(
                    work.clone(),
                    ctrl.clone(),
                    stats.clone(),
                    errors.clone(),
                    running.clone(),
                )
            })
            .collect();

        // Snapshot task runs from the start (including ramp)
        let snapshot_handle = spawn_snapshot_task(
            stats.clone(),
            running.clone(),
            in_ramp.clone(),
            self.show_ramp_progress,
            self.snapshot_interval,
            self.csv_path.clone(),
            self.show_progress,
            self.progress_fn,
        );

        // Ramp-up phase
        if let Some(ramp_dur) = self.ramp_up {
            match self.rate_mode {
                RateMode::Total(target) => {
                    ramp_drive(rate_ctrls[0].clone(), self.ramp_start_rate, target, ramp_dur).await;
                }
                RateMode::PerWorker(_) => {
                    ramp_drive_many(rate_ctrls, ramp_target.0, ramp_target.1, ramp_dur).await;
                }
            }
            tracing::info!("Ramp-up complete, starting measurement");
            in_ramp.store(false, Ordering::SeqCst);
        }

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

        BenchmarkResults {
            summary: BenchmarkSummary::from_snapshot(&snap, error_counts),
            target_rate: total_rate,
            workers: self.workers,
            duration: self.duration,
            csv_path: self.csv_path,
        }
    }
}

// ── Internal helpers ─────────────────────────────────────────────────────────

fn log_config(rate_mode: &RateMode, workers: usize, duration: &Duration) {
    if rate_mode.is_unlimited() {
        tracing::info!(
            "Benchmark: {} workers, UNLIMITED rate, {}s duration",
            workers,
            duration.as_secs()
        );
    } else {
        let (total, label) = match rate_mode {
            RateMode::Total(r) => (*r, "shared"),
            RateMode::PerWorker(r) => (*r * workers as f64, "per-worker"),
        };
        tracing::info!(
            "Benchmark: {} workers, {:.1} req/s ({}), {}s duration",
            workers,
            total,
            label,
            duration.as_secs()
        );
    }
}

/// Build one controller per worker.
///
/// For `Total` mode every entry is a clone of the same underlying controller
/// (Arc-backed sharing). For `PerWorker` each entry is independent.
///
/// Returns `(controllers, (per_worker_start_rate, per_worker_target_rate))`
/// where the second tuple is used by `ramp_drive_many` for PerWorker mode.
fn build_rate_controllers(
    mode: &RateMode,
    workers: usize,
    has_ramp: bool,
    ramp_start_rate: f64,
    burst_factor: f64,
) -> (Vec<DynamicRateController>, (f64, f64)) {
    match mode {
        RateMode::Total(rate) => {
            let start = if has_ramp { ramp_start_rate } else { *rate };
            let ctrl = DynamicRateController::with_burst(start, burst_factor);
            (vec![ctrl; workers], (ramp_start_rate, *rate))
        }
        RateMode::PerWorker(rate) => {
            let start = if has_ramp {
                ramp_start_rate / workers as f64
            } else {
                *rate
            };
            let ctrls = (0..workers)
                .map(|_| DynamicRateController::with_burst(start, burst_factor))
                .collect();
            (ctrls, (start, *rate))
        }
    }
}

/// Spawn a single request/response worker task.
fn spawn_worker<W: BenchmarkWork>(
    work: W,
    rate_ctrl: DynamicRateController,
    stats: Arc<Stats>,
    errors: ErrorCounter,
    running: Arc<AtomicBool>,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        let mut state = work.init().await;
        let mut lat_buf: Vec<u64> = Vec::with_capacity(64);
        while running.load(Ordering::Relaxed) {
            rate_ctrl.acquire().await;
            stats.record_sent().await;
            match work.work(&mut state).await {
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
        work.cleanup(state).await;
    })
}

/// Spawn the periodic snapshot / CSV / progress task.
///
/// While `in_ramp` is true, lines are prefixed with `[RAMP]` (or suppressed
/// entirely when `show_ramp_progress` is false).
fn spawn_snapshot_task(
    stats: Arc<Stats>,
    running: Arc<AtomicBool>,
    in_ramp: Arc<AtomicBool>,
    show_ramp_progress: bool,
    interval: Duration,
    csv_path: Option<PathBuf>,
    show_progress: bool,
    progress_fn: ProgressFn,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        let mut ticker = tokio::time::interval(interval);
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

        while running.load(Ordering::Relaxed) {
            ticker.tick().await;
            let snapshot = stats.snapshot().await;
            let is_ramp = in_ramp.load(Ordering::Relaxed);

            if let Some(w) = csv_writer.as_mut() {
                writeln!(w, "{}", snapshot.to_csv_row()).ok();
            }

            if is_ramp && !show_ramp_progress {
                continue;
            }

            if show_progress {
                if let Some(line) = progress_fn(&snapshot) {
                    if is_ramp {
                        crate::tprintln!("[RAMP] {}", line);
                    } else {
                        crate::tprintln!("{}", line);
                    }
                }
            } else {
                println!("{}", snapshot.to_csv_row());
            }
        }
    })
}

// ── Ramp helpers (pub(crate) so other patterns can reuse) ────────────────────

/// Linearly ramp a single shared [`DynamicRateController`] from `start_rate` to
/// `target_rate` over `duration`, updating every ~100 ms.
pub(crate) async fn ramp_drive(
    ctrl: DynamicRateController,
    start_rate: f64,
    target_rate: f64,
    duration: Duration,
) {
    const STEP: Duration = Duration::from_millis(100);
    let steps = (duration.as_secs_f64() / STEP.as_secs_f64()).ceil() as u64;
    if steps == 0 {
        ctrl.set_rate(target_rate);
        return;
    }
    ctrl.set_rate(start_rate);
    for i in 1..=steps {
        tokio::time::sleep(STEP).await;
        let t = (i as f64 / steps as f64).min(1.0);
        let rate = start_rate + (target_rate - start_rate) * t;
        ctrl.set_rate(rate);
    }
}

/// Linearly ramp a collection of per-worker [`DynamicRateController`]s, each
/// from `start_rate` to `target_rate` over `duration`.
pub(crate) async fn ramp_drive_many(
    ctrls: Vec<DynamicRateController>,
    start_rate: f64,
    target_rate: f64,
    duration: Duration,
) {
    const STEP: Duration = Duration::from_millis(100);
    let steps = (duration.as_secs_f64() / STEP.as_secs_f64()).ceil() as u64;
    if steps == 0 {
        for c in &ctrls {
            c.set_rate(target_rate);
        }
        return;
    }
    for c in &ctrls {
        c.set_rate(start_rate);
    }
    for i in 1..=steps {
        tokio::time::sleep(STEP).await;
        let t = (i as f64 / steps as f64).min(1.0);
        let rate = start_rate + (target_rate - start_rate) * t;
        for c in &ctrls {
            c.set_rate(rate);
        }
    }
}
