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

use crate::metrics::errors::ErrorCounter;
use crate::patterns::work::{AsyncTaskResults, BenchmarkSummary, PollResult, PollWork, SubmitWork};
use crate::rate::RateController;
use crate::Stats;
use std::io::Write;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::RwLock;

/// Builder for async task benchmarks.
///
/// Submit workers create tasks at a controlled rate; poll workers check for
/// completion and record latency. Set both a [`SubmitWork`] and a [`PollWork`]
/// implementation via [`.submitter()`] and [`.poller()`] before calling [`.run()`].
pub struct AsyncTaskBenchmark<S = (), P = ()> {
    submit_workers: usize,
    poll_workers: usize,
    rate: f64,
    duration: Duration,
    csv_path: Option<PathBuf>,
    show_progress: bool,
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
            duration: Duration::from_secs(10),
            csv_path: None,
            show_progress: true,
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

    /// Set the submit implementation.
    ///
    /// The framework clones `submitter` once per submit worker. Put shared
    /// resources (HTTP clients, connection pools) in the struct; per-worker
    /// resources go in [`SubmitWork::State`] via [`SubmitWork::init`].
    pub fn submitter<S2: SubmitWork>(self, s: S2) -> AsyncTaskBenchmark<S2, P> {
        AsyncTaskBenchmark {
            submit_workers: self.submit_workers,
            poll_workers: self.poll_workers,
            rate: self.rate,
            duration: self.duration,
            csv_path: self.csv_path,
            show_progress: self.show_progress,
            submitter: Some(s),
            poller: self.poller,
        }
    }

    /// Set the poll implementation.
    ///
    /// The framework clones `poller` once per poll worker.
    pub fn poller<P2: PollWork>(self, p: P2) -> AsyncTaskBenchmark<S, P2> {
        AsyncTaskBenchmark {
            submit_workers: self.submit_workers,
            poll_workers: self.poll_workers,
            rate: self.rate,
            duration: self.duration,
            csv_path: self.csv_path,
            show_progress: self.show_progress,
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
        let pending: Arc<RwLock<Vec<u64>>> = Arc::new(RwLock::new(Vec::new()));
        let errors = ErrorCounter::new();

        tracing::info!(
            "AsyncTask: {} submit @ {:.0}/s, {} poll, {}s",
            self.submit_workers,
            self.rate,
            self.poll_workers,
            self.duration.as_secs()
        );

        let mut handles = Vec::new();

        // ---- Submit workers -----------------------------------------------
        let rate_per_submitter = self.rate / self.submit_workers as f64;
        for _ in 0..self.submit_workers {
            let s = submitter.clone();
            let stats = submit_stats.clone();
            let pending = pending.clone();
            let running = running.clone();
            let errors = errors.clone();
            handles.push(tokio::spawn(async move {
                let mut state = s.init().await;
                let mut rate_ctrl = RateController::new(rate_per_submitter);
                while running.load(Ordering::Relaxed) {
                    rate_ctrl.wait_for_next().await;
                    stats.record_sent().await;
                    match s.submit(&mut state).await {
                        Some(task_id) => {
                            pending.write().await.push(task_id);
                        }
                        None => {
                            errors.record("submit failed").await;
                            stats.record_error().await;
                        }
                    }
                }
                s.cleanup(state).await;
            }));
        }

        // ---- Poll workers -------------------------------------------------
        for _ in 0..self.poll_workers {
            let p = poller.clone();
            let stats = complete_stats.clone();
            let pending = pending.clone();
            let running = running.clone();
            let errors = errors.clone();
            handles.push(tokio::spawn(async move {
                let mut state = p.init().await;
                while running.load(Ordering::Relaxed) {
                    let task_id = {
                        let mut tasks = pending.write().await;
                        tasks.pop()
                    };
                    match task_id {
                        Some(id) => match p.poll(id, &mut state).await {
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
                p.cleanup(state).await;
            }));
        }

        // ---- Snapshot task ------------------------------------------------
        let ss = submit_stats.clone();
        let cs = complete_stats.clone();
        let snap_running = running.clone();
        let csv_path = self.csv_path.clone();
        let show_progress = self.show_progress;
        let snapshot_handle = tokio::spawn(async move {
            let mut ticker = tokio::time::interval(Duration::from_secs(1));
            let mut csv_writer = csv_path.as_ref().map(|p| {
                let f = std::fs::File::create(p).expect("Failed to create CSV file");
                std::io::BufWriter::new(f)
            });

            let header = "timestamp,submitted,completed,in_flight,throughput,p50_ms,p95_ms,p99_ms";
            if let Some(w) = csv_writer.as_mut() {
                writeln!(w, "{}", header).ok();
            }
            if !show_progress {
                println!("{}", header);
            }

            while snap_running.load(Ordering::Relaxed) {
                ticker.tick().await;
                let s = ss.snapshot().await;
                let c = cs.snapshot().await;
                let in_flight = s.sent_count.saturating_sub(c.received_count);

                let row = format!(
                    "{},{},{},{},{:.2},{:.3},{:.3},{:.3}",
                    s.timestamp,
                    s.sent_count,
                    c.received_count,
                    in_flight,
                    c.interval_throughput(),
                    c.latency_ns_p50 as f64 / 1_000_000.0,
                    c.latency_ns_p95 as f64 / 1_000_000.0,
                    c.latency_ns_p99 as f64 / 1_000_000.0,
                );

                if let Some(w) = csv_writer.as_mut() {
                    writeln!(w, "{}", row).ok();
                }

                if show_progress {
                    eprint!(
                        "\r  {} submitted | {} completed | {} in-flight | p50={:.1}ms  ",
                        s.sent_count,
                        c.received_count,
                        in_flight,
                        c.latency_ns_p50 as f64 / 1_000_000.0
                    );
                } else {
                    println!("{}", row);
                }
            }
            if show_progress {
                eprintln!();
            }
        });

        // ---- Wait and collect ---------------------------------------------
        tokio::time::sleep(self.duration).await;
        running.store(false, Ordering::SeqCst);

        for handle in handles {
            let _ = handle.await;
        }
        let _ = snapshot_handle.await;

        let ss = submit_stats.snapshot().await;
        let cs = complete_stats.snapshot().await;
        let error_counts = errors.take().await;
        let ns = |v: u64| v as f64 / 1_000_000.0;

        let submitted = BenchmarkSummary {
            total_ops: ss.sent_count,
            successes: ss.sent_count.saturating_sub(ss.error_count),
            errors: ss.error_count,
            throughput: ss.total_throughput(),
            latency_p25_ms: 0.0,
            latency_p50_ms: 0.0,
            latency_p75_ms: 0.0,
            latency_p95_ms: 0.0,
            latency_p99_ms: 0.0,
            latency_min_ms: 0.0,
            latency_max_ms: 0.0,
            latency_mean_ms: 0.0,
            latency_stddev_ms: 0.0,
            latency_samples: 0,
            error_breakdown: error_counts,
        };

        let completed = BenchmarkSummary {
            total_ops: cs.received_count,
            successes: cs.received_count,
            errors: cs.error_count,
            throughput: cs.total_throughput(),
            latency_p25_ms: ns(cs.latency_ns_p25),
            latency_p50_ms: ns(cs.latency_ns_p50),
            latency_p75_ms: ns(cs.latency_ns_p75),
            latency_p95_ms: ns(cs.latency_ns_p95),
            latency_p99_ms: ns(cs.latency_ns_p99),
            latency_min_ms: ns(cs.latency_ns_min),
            latency_max_ms: ns(cs.latency_ns_max),
            latency_mean_ms: cs.latency_ns_mean / 1_000_000.0,
            latency_stddev_ms: cs.latency_ns_stddev / 1_000_000.0,
            latency_samples: cs.latency_sample_count,
            error_breakdown: Default::default(),
        };

        AsyncTaskResults {
            submitted,
            completed,
            submit_workers: self.submit_workers,
            poll_workers: self.poll_workers,
            target_rate: self.rate,
            duration: self.duration,
            csv_path: self.csv_path,
        }
    }
}
