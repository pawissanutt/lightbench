//! Async task benchmark pattern.
//!
//! Submit workers create tasks at a controlled rate; poll workers check for
//! completion. The framework tracks end-to-end task latency.
//!
//! Supply a [`SubmitWork`] and a [`PollWork`] implementation via
//! [`.submitter()`] and [`.poller()`] before calling [`.run()`].
//!
//! # Example
//!
//! ```ignore
//! use lightbench::{AsyncTaskBenchmark, PollResult, SubmitWork, PollWork};
//!
//! #[derive(Clone)]
//! struct MySubmitter { client: reqwest::Client, url: String }
//!
//! impl SubmitWork for MySubmitter {
//!     type State = ();
//!     async fn init(&self) -> () {}
//!     async fn submit(&self, _: &mut ()) -> Option<u64> {
//!         // POST task, return Some(task_id) or None on failure
//!         Some(42)
//!     }
//! }
//!
//! #[derive(Clone)]
//! struct MyPoller { client: reqwest::Client, base_url: String }
//!
//! impl PollWork for MyPoller {
//!     type State = ();
//!     async fn init(&self) -> () {}
//!     async fn poll(&self, task_id: u64, _: &mut ()) -> PollResult {
//!         PollResult::Completed { latency_ns: 1_000_000 }
//!     }
//! }
//!
//! let results = AsyncTaskBenchmark::new()
//!     .submit_workers(4)
//!     .poll_workers(4)
//!     .rate(500.0)
//!     .duration_secs(10)
//!     .submitter(MySubmitter { client, url: submit_url })
//!     .poller(MyPoller { client, base_url: poll_url })
//!     .run()
//!     .await;
//!
//! results.print_summary();
//! ```

use crate::Stats;
use crate::metrics::StatsSnapshot;
use crate::metrics::errors::ErrorCounter;
use crate::patterns::request::ramp_drive;
use crate::patterns::work::{AsyncTaskResults, BenchmarkSummary, PollResult, PollWork, SubmitWork};
use crate::patterns::{DualProgressFn, DualSnapshotConfig, spawn_dual_snapshot_task};
use crate::rate::DynamicRateController;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;
use tokio::sync::RwLock;

type ProgressFn = DualProgressFn;

/// Builder for async task benchmarks.
///
/// Submit workers create tasks at a controlled rate; poll workers check for
/// completion and record latency. Set both a [`SubmitWork`] and a [`PollWork`]
/// implementation via [`.submitter()`] and [`.poller()`] before calling [`.run()`].
pub struct AsyncTaskBenchmark<S = (), P = ()> {
    submit_workers: usize,
    poll_workers: usize,
    rate: f64,
    ramp_up: Option<Duration>,
    ramp_start_rate: f64,
    burst_factor: f64,
    show_ramp_progress: bool,
    duration: Duration,
    drain_timeout: Option<Duration>,
    csv_path: Option<PathBuf>,
    show_progress: bool,
    progress_fn: ProgressFn,
    submitter: Option<S>,
    poller: Option<P>,
}

impl Default for AsyncTaskBenchmark<(), ()> {
    fn default() -> Self {
        Self::new()
    }
}

impl AsyncTaskBenchmark<(), ()> {
    /// Create with defaults: 2 submit workers, 2 poll workers, 500 req/s, 10s duration.
    pub fn new() -> Self {
        Self {
            submit_workers: 2,
            poll_workers: 2,
            rate: 500.0,
            ramp_up: None,
            ramp_start_rate: 0.0,
            burst_factor: 0.1,
            show_ramp_progress: true,
            duration: Duration::from_secs(10),
            drain_timeout: Some(Duration::from_secs(30)),
            csv_path: None,
            show_progress: true,
            progress_fn: Arc::new(|s, c| {
                let in_flight = s.sent_count.saturating_sub(c.received_count);
                Some(format!(
                    "  {} submitted | {} completed | {} in-flight | p50={:.1}ms",
                    s.sent_count,
                    c.received_count,
                    in_flight,
                    c.latency_ns_p50 as f64 / 1_000_000.0
                ))
            }),
            submitter: None,
            poller: None,
        }
    }
}

impl<S, P> AsyncTaskBenchmark<S, P> {
    /// Set number of submit workers.
    pub fn submit_workers(mut self, n: usize) -> Self {
        self.submit_workers = n.max(1);
        self
    }

    /// Set number of poll workers.
    pub fn poll_workers(mut self, n: usize) -> Self {
        self.poll_workers = n.max(1);
        self
    }

    /// Set total submit rate (req/s, shared across all submit workers).
    pub fn rate(mut self, rate: f64) -> Self {
        self.rate = rate;
        self
    }

    /// Set the ramp-up duration before the measured benchmark starts.
    pub fn ramp_up(mut self, duration: Duration) -> Self {
        self.ramp_up = Some(duration);
        self
    }

    /// Set the initial rate at the start of the ramp-up period (default: `0.0`).
    pub fn ramp_start_rate(mut self, rate: f64) -> Self {
        self.ramp_start_rate = rate.max(0.0);
        self
    }

    /// Set the burst factor for the rate controller (default: `0.1`).
    pub fn burst_factor(mut self, factor: f64) -> Self {
        self.burst_factor = factor.clamp(0.001, 1000.0);
        self
    }

    /// Show progress during the ramp-up period (default: `true`).
    pub fn show_ramp_progress(mut self, show: bool) -> Self {
        self.show_ramp_progress = show;
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

    /// Wait for all pending tasks to drain after the benchmark duration ends.
    ///
    /// Set the maximum time to wait for in-flight tasks to complete.
    /// Pass `None` to disable draining (default: 30s).
    pub fn drain_timeout(mut self, timeout: Option<Duration>) -> Self {
        self.drain_timeout = timeout;
        self
    }

    /// Wait for all pending tasks to drain with the given timeout in seconds.
    pub fn drain_timeout_secs(mut self, secs: u64) -> Self {
        self.drain_timeout = Some(Duration::from_secs(secs));
        self
    }

    /// Set CSV output file path.
    pub fn csv<PP: Into<PathBuf>>(mut self, path: PP) -> Self {
        self.csv_path = Some(path.into());
        self
    }

    /// Enable/disable progress display.
    pub fn progress(mut self, show: bool) -> Self {
        self.show_progress = show;
        self
    }

    /// Set a custom progress formatter.
    ///
    /// The closure receives the submit and complete [`StatsSnapshot`]s.
    pub fn on_progress<F>(mut self, f: F) -> Self
    where
        F: Fn(&StatsSnapshot, &StatsSnapshot) -> Option<String> + Send + Sync + 'static,
    {
        self.progress_fn = Arc::new(f);
        self
    }

    /// Set the submit implementation.
    pub fn submitter<S2: SubmitWork>(self, s: S2) -> AsyncTaskBenchmark<S2, P> {
        AsyncTaskBenchmark {
            submit_workers: self.submit_workers,
            poll_workers: self.poll_workers,
            rate: self.rate,
            ramp_up: self.ramp_up,
            ramp_start_rate: self.ramp_start_rate,
            burst_factor: self.burst_factor,
            show_ramp_progress: self.show_ramp_progress,
            duration: self.duration,
            drain_timeout: self.drain_timeout,
            csv_path: self.csv_path,
            show_progress: self.show_progress,
            progress_fn: self.progress_fn,
            submitter: Some(s),
            poller: self.poller,
        }
    }

    /// Set the poll implementation.
    pub fn poller<P2: PollWork>(self, p: P2) -> AsyncTaskBenchmark<S, P2> {
        AsyncTaskBenchmark {
            submit_workers: self.submit_workers,
            poll_workers: self.poll_workers,
            rate: self.rate,
            ramp_up: self.ramp_up,
            ramp_start_rate: self.ramp_start_rate,
            burst_factor: self.burst_factor,
            show_ramp_progress: self.show_ramp_progress,
            duration: self.duration,
            drain_timeout: self.drain_timeout,
            csv_path: self.csv_path,
            show_progress: self.show_progress,
            progress_fn: self.progress_fn,
            submitter: self.submitter,
            poller: Some(p),
        }
    }
}

impl<S: SubmitWork, P: PollWork> AsyncTaskBenchmark<S, P> {
    /// Run the benchmark.
    pub async fn run(self) -> AsyncTaskResults {
        let submitter = self.submitter.expect("call .submitter() before .run()");
        let poller = self.poller.expect("call .poller() before .run()");

        let submit_stats = Arc::new(Stats::new());
        let complete_stats = Arc::new(Stats::new());
        let running = Arc::new(AtomicBool::new(true));
        let submitting = Arc::new(AtomicBool::new(true));
        let pending: Arc<RwLock<Vec<u64>>> = Arc::new(RwLock::new(Vec::new()));
        let errors = ErrorCounter::new();
        let in_ramp = Arc::new(AtomicBool::new(self.ramp_up.is_some()));

        tracing::info!(
            "AsyncTask: {} submit @ {:.0}/s, {} poll, {}s",
            self.submit_workers,
            self.rate,
            self.poll_workers,
            self.duration.as_secs()
        );

        let show_progress = self.show_progress;
        let csv_path = self.csv_path.clone();
        let progress_fn = self.progress_fn;
        let mut submit_handles = Vec::new();
        let mut poll_handles = Vec::new();

        // ---- Submit workers -----------------------------------------------
        let start_rate = if self.ramp_up.is_some() {
            self.ramp_start_rate
        } else {
            self.rate
        };
        let rate_ctrl = DynamicRateController::with_burst(start_rate, self.burst_factor);

        for _ in 0..self.submit_workers {
            submit_handles.push(spawn_submit_worker(
                submitter.clone(),
                rate_ctrl.clone(),
                submit_stats.clone(),
                pending.clone(),
                errors.clone(),
                submitting.clone(),
            ));
        }

        // ---- Poll workers -------------------------------------------------
        for _ in 0..self.poll_workers {
            poll_handles.push(spawn_poll_worker(
                poller.clone(),
                complete_stats.clone(),
                pending.clone(),
                errors.clone(),
                running.clone(),
            ));
        }

        // ---- Snapshot task (runs from the start, including during ramp) ---
        let snapshot_handle = if show_progress || csv_path.is_some() {
            Some(spawn_dual_snapshot_task(
                submit_stats.clone(),
                complete_stats.clone(),
                running.clone(),
                DualSnapshotConfig {
                    in_ramp: in_ramp.clone(),
                    show_ramp_progress: self.show_ramp_progress,
                    csv_path,
                    show_progress,
                    progress_fn,
                    csv_header: "timestamp,submitted,completed,in_flight,throughput,p50_ms,p95_ms,p99_ms",
                    csv_row_fn: |s, c| {
                        let in_flight = s.sent_count.saturating_sub(c.received_count);
                        format!(
                            "{},{},{},{},{:.2},{:.3},{:.3},{:.3}",
                            s.timestamp,
                            s.sent_count,
                            c.received_count,
                            in_flight,
                            c.interval_throughput(),
                            c.latency_ns_p50 as f64 / 1_000_000.0,
                            c.latency_ns_p95 as f64 / 1_000_000.0,
                            c.latency_ns_p99 as f64 / 1_000_000.0,
                        )
                    },
                },
            ))
        } else {
            None
        };

        // ---- Ramp-up ----------------------------------------------------
        if let Some(ramp_dur) = self.ramp_up {
            ramp_drive(rate_ctrl, self.ramp_start_rate, self.rate, ramp_dur).await;
            tracing::info!("Ramp-up complete, starting measurement");
            in_ramp.store(false, Ordering::SeqCst);
        }

        // ---- Wait for benchmark duration -----------------------------------
        tokio::time::sleep(self.duration).await;
        submitting.store(false, Ordering::SeqCst);

        for handle in submit_handles {
            let _ = handle.await;
        }

        // ---- Drain pending tasks -------------------------------------------
        if let Some(drain_dur) = self.drain_timeout {
            tracing::info!(
                "Benchmark duration complete, draining pending tasks (timeout: {}s)",
                drain_dur.as_secs()
            );
            let deadline = tokio::time::Instant::now() + drain_dur;
            loop {
                let remaining = pending.read().await.len();
                if remaining == 0 {
                    tracing::info!("All pending tasks drained");
                    break;
                }
                if tokio::time::Instant::now() >= deadline {
                    tracing::warn!("{} tasks still pending after drain timeout", remaining);
                    break;
                }
                tokio::time::sleep(Duration::from_millis(50)).await;
            }
        }

        running.store(false, Ordering::SeqCst);

        for handle in poll_handles {
            let _ = handle.await;
        }
        if let Some(h) = snapshot_handle {
            let _ = h.await;
        }

        let ss = submit_stats.snapshot().await;
        let cs = complete_stats.snapshot().await;
        let error_counts = errors.take().await;

        AsyncTaskResults {
            submitted: BenchmarkSummary::from_snapshot_send_only(&ss, error_counts),
            completed: BenchmarkSummary::from_snapshot_recv_only(&cs),
            submit_workers: self.submit_workers,
            poll_workers: self.poll_workers,
            target_rate: self.rate,
            duration: self.duration,
            csv_path: self.csv_path,
        }
    }
}

// ── Internal helpers ─────────────────────────────────────────────────────────

fn spawn_submit_worker<S: SubmitWork>(
    submitter: S,
    rate_ctrl: DynamicRateController,
    stats: Arc<Stats>,
    pending: Arc<RwLock<Vec<u64>>>,
    errors: ErrorCounter,
    running: Arc<AtomicBool>,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        let mut state = submitter.init().await;
        while running.load(Ordering::Relaxed) {
            rate_ctrl.acquire().await;
            stats.record_sent().await;
            match submitter.submit(&mut state).await {
                Some(task_id) => {
                    pending.write().await.push(task_id);
                }
                None => {
                    errors.record("submit failed").await;
                    stats.record_error().await;
                }
            }
        }
        submitter.cleanup(state).await;
    })
}

fn spawn_poll_worker<P: PollWork>(
    poller: P,
    stats: Arc<Stats>,
    pending: Arc<RwLock<Vec<u64>>>,
    errors: ErrorCounter,
    running: Arc<AtomicBool>,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        let mut state = poller.init().await;
        while running.load(Ordering::Relaxed) {
            let task_id = {
                let mut tasks = pending.write().await;
                tasks.pop()
            };
            match task_id {
                Some(id) => match poller.poll(id, &mut state).await {
                    PollResult::Completed { latency_ns } => {
                        stats.record_received(latency_ns).await;
                    }
                    PollResult::Pending => {
                        pending.write().await.push(id);
                        tokio::time::sleep(Duration::from_micros(100)).await;
                    }
                    PollResult::Error(reason) => {
                        tracing::debug!(reason, "poll error");
                        errors.record(&reason).await;
                        stats.record_error().await;
                    }
                },
                None => {
                    tokio::task::yield_now().await;
                }
            }
        }
        poller.cleanup(state).await;
    })
}
