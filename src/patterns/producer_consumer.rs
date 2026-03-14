//! Producer/Consumer benchmark pattern.
//!
//! Rate-controlled producers write items; consumers own their event loop
//! and report consumed items back to the framework via a [`ConsumerRecorder`](crate::ConsumerRecorder).
//! This design suits subscription-based APIs where messages are pushed to the
//! consumer (e.g. message brokers, event streams).
//!
//! Supply a [`ProducerWork`] and a [`ConsumerWork`] implementation; the
//! framework handles rate control, stats, progress, and CSV export.
//!
//! # Example
//!
//! ```ignore
//! use lightbench::{
//!     ProducerConsumerBenchmark, ProducerWork, ConsumerWork, ConsumerRecorder, now_unix_ns_estimate,
//! };
//! use std::collections::VecDeque;
//! use std::sync::Arc;
//! use tokio::sync::Mutex;
//!
//! type Queue = Arc<Mutex<VecDeque<u64>>>;
//!
//! #[derive(Clone)]
//! struct QueueProducer { queue: Queue }
//!
//! impl ProducerWork for QueueProducer {
//!     type State = ();
//!     async fn init(&self) -> () {}
//!     async fn produce(&self, _: &mut ()) -> Result<(), String> {
//!         self.queue.lock().await.push_back(now_unix_ns_estimate());
//!         Ok(())
//!     }
//! }
//!
//! #[derive(Clone)]
//! struct QueueConsumer { queue: Queue }
//!
//! impl ConsumerWork for QueueConsumer {
//!     type State = ();
//!     async fn init(&self) -> () {}
//!     async fn run(&self, _state: (), recorder: ConsumerRecorder) {
//!         while recorder.is_running() {
//!             let item = self.queue.lock().await.pop_front();
//!             match item {
//!                 Some(ts) => {
//!                     recorder.record(now_unix_ns_estimate().saturating_sub(ts)).await;
//!                 }
//!                 None => tokio::task::yield_now().await,
//!             }
//!         }
//!     }
//! }
//!
//! let results = ProducerConsumerBenchmark::new()
//!     .producers(4)
//!     .consumers(4)
//!     .rate(10_000.0)
//!     .duration_secs(10)
//!     .producer(QueueProducer { queue: queue.clone() })
//!     .consumer(QueueConsumer { queue: queue.clone() })
//!     .run()
//!     .await;
//!
//! results.print_summary();
//! ```

use crate::Stats;
use crate::metrics::StatsSnapshot;
use crate::metrics::errors::ErrorCounter;
use crate::patterns::request::ramp_drive;
use crate::patterns::work::{
    BenchmarkSummary, ConsumerWork, ProducerConsumerResults, ProducerWork,
};
use crate::patterns::{DualProgressFn, DualSnapshotConfig, spawn_dual_snapshot_task};
use crate::rate::DynamicRateController;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

type ProgressFn = DualProgressFn;

/// Builder for producer/consumer benchmarks.
///
/// Producers are rate-controlled; consumers own their event loop and report
/// stats via a [`ConsumerRecorder`](crate::ConsumerRecorder) callback.
///
/// Set both a [`ProducerWork`] and a [`ConsumerWork`] via [`.producer()`] and
/// [`.consumer()`] before calling [`.run()`].
pub struct ProducerConsumerBenchmark<PR = (), CO = ()> {
    producers: usize,
    consumers: usize,
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
    producer: Option<PR>,
    consumer: Option<CO>,
}

impl Default for ProducerConsumerBenchmark<(), ()> {
    fn default() -> Self {
        Self::new()
    }
}

impl ProducerConsumerBenchmark<(), ()> {
    /// Create a new producer/consumer benchmark with defaults.
    ///
    /// Defaults: 1 producer, 1 consumer, 1000 msg/s, 10s duration.
    pub fn new() -> Self {
        Self {
            producers: 1,
            consumers: 1,
            rate: 1000.0,
            ramp_up: None,
            ramp_start_rate: 0.0,
            burst_factor: 0.1,
            show_ramp_progress: true,
            duration: Duration::from_secs(10),
            drain_timeout: Some(Duration::from_secs(30)),
            csv_path: None,
            show_progress: true,
            progress_fn: Arc::new(|p, c| {
                let in_flight = p.sent_count.saturating_sub(c.received_count);
                Some(format!(
                    "  {} produced | {} consumed | {} in-flight | p50={:.2}ms",
                    p.sent_count,
                    c.received_count,
                    in_flight,
                    c.latency_ns_p50 as f64 / 1_000_000.0
                ))
            }),
            producer: None,
            consumer: None,
        }
    }
}

impl<PR, CO> ProducerConsumerBenchmark<PR, CO> {
    /// Set number of producer workers.
    pub fn producers(mut self, n: usize) -> Self {
        self.producers = n.max(1);
        self
    }

    /// Set number of consumer workers.
    pub fn consumers(mut self, n: usize) -> Self {
        self.consumers = n.max(1);
        self
    }

    /// Set total produce rate (msg/s, shared across all producers).
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

    /// Wait for all in-flight items to drain after the benchmark duration ends.
    ///
    /// Set the maximum time to wait for consumers to finish processing.
    /// Pass `None` to disable draining (default: 30s).
    pub fn drain_timeout(mut self, timeout: Option<Duration>) -> Self {
        self.drain_timeout = timeout;
        self
    }

    /// Wait for all in-flight items to drain with the given timeout in seconds.
    pub fn drain_timeout_secs(mut self, secs: u64) -> Self {
        self.drain_timeout = Some(Duration::from_secs(secs));
        self
    }

    /// Set CSV output file path.
    pub fn csv<P: Into<PathBuf>>(mut self, path: P) -> Self {
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
    /// The closure receives the producer and consumer [`StatsSnapshot`]s.
    pub fn on_progress<F>(mut self, f: F) -> Self
    where
        F: Fn(&StatsSnapshot, &StatsSnapshot) -> Option<String> + Send + Sync + 'static,
    {
        self.progress_fn = Arc::new(f);
        self
    }

    /// Set the producer implementation.
    pub fn producer<PR2: ProducerWork>(self, p: PR2) -> ProducerConsumerBenchmark<PR2, CO> {
        ProducerConsumerBenchmark {
            producers: self.producers,
            consumers: self.consumers,
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
            producer: Some(p),
            consumer: self.consumer,
        }
    }

    /// Set the consumer implementation.
    pub fn consumer<CO2: ConsumerWork>(self, c: CO2) -> ProducerConsumerBenchmark<PR, CO2> {
        ProducerConsumerBenchmark {
            producers: self.producers,
            consumers: self.consumers,
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
            producer: self.producer,
            consumer: Some(c),
        }
    }
}

impl<PR: ProducerWork, CO: ConsumerWork> ProducerConsumerBenchmark<PR, CO> {
    /// Run the benchmark.
    pub async fn run(self) -> ProducerConsumerResults {
        let producer = self.producer.expect("call .producer() before .run()");
        let consumer = self.consumer.expect("call .consumer() before .run()");

        let producer_stats = Arc::new(Stats::new());
        let consumer_stats = Arc::new(Stats::new());
        let errors = ErrorCounter::new();
        let running = Arc::new(AtomicBool::new(true));
        let producing = Arc::new(AtomicBool::new(true));
        let in_ramp = Arc::new(AtomicBool::new(self.ramp_up.is_some()));

        tracing::info!(
            "ProducerConsumer: {} producers @ {:.0} msg/s, {} consumers, {}s",
            self.producers,
            self.rate,
            self.consumers,
            self.duration.as_secs()
        );

        let show_progress = self.show_progress;
        let csv_path = self.csv_path.clone();
        let progress_fn = self.progress_fn;
        let mut producer_handles = Vec::new();
        let mut consumer_handles = Vec::new();

        // ---- Producers --------------------------------------------------
        let start_rate = if self.ramp_up.is_some() {
            self.ramp_start_rate
        } else {
            self.rate
        };
        let rate_ctrl = DynamicRateController::with_burst(start_rate, self.burst_factor);

        for _ in 0..self.producers {
            producer_handles.push(spawn_producer(
                producer.clone(),
                rate_ctrl.clone(),
                producer_stats.clone(),
                errors.clone(),
                producing.clone(),
            ));
        }

        // ---- Consumers --------------------------------------------------
        for _ in 0..self.consumers {
            consumer_handles.push(spawn_consumer(
                consumer.clone(),
                consumer_stats.clone(),
                running.clone(),
            ));
        }

        // ---- Snapshot task (runs from the start, including during ramp) -
        let snapshot_handle = if show_progress || csv_path.is_some() {
            Some(spawn_dual_snapshot_task(
                producer_stats.clone(),
                consumer_stats.clone(),
                running.clone(),
                DualSnapshotConfig {
                    in_ramp: in_ramp.clone(),
                    show_ramp_progress: self.show_ramp_progress,
                    csv_path,
                    show_progress,
                    progress_fn,
                    csv_header: "timestamp,produced,consumed,in_flight,produce_rate,consume_rate,p50_ms,p95_ms,p99_ms",
                    csv_row_fn: |p, c| {
                        let in_flight = p.sent_count.saturating_sub(c.received_count);
                        format!(
                            "{},{},{},{},{:.2},{:.2},{:.3},{:.3},{:.3}",
                            p.timestamp,
                            p.sent_count,
                            c.received_count,
                            in_flight,
                            p.interval_throughput(),
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

        // ---- Wait and collect -------------------------------------------
        tokio::time::sleep(self.duration).await;
        producing.store(false, Ordering::SeqCst);

        for handle in producer_handles {
            let _ = handle.await;
        }

        // ---- Drain in-flight items --------------------------------------
        if let Some(drain_dur) = self.drain_timeout {
            let produced = producer_stats.snapshot().await.sent_count;
            if produced > consumer_stats.snapshot().await.received_count {
                tracing::info!(
                    "Benchmark duration complete, draining in-flight items (timeout: {}s)",
                    drain_dur.as_secs()
                );
                let deadline = tokio::time::Instant::now() + drain_dur;
                loop {
                    let consumed = consumer_stats.snapshot().await.received_count;
                    if consumed >= produced {
                        tracing::info!("All in-flight items drained");
                        break;
                    }
                    if tokio::time::Instant::now() >= deadline {
                        tracing::warn!(
                            "{} items still in-flight after drain timeout",
                            produced.saturating_sub(consumed)
                        );
                        break;
                    }
                    tokio::time::sleep(Duration::from_millis(50)).await;
                }
            }
        }

        running.store(false, Ordering::SeqCst);

        for handle in consumer_handles {
            let _ = handle.await;
        }
        if let Some(h) = snapshot_handle {
            let _ = h.await;
        }

        let ps = producer_stats.snapshot().await;
        let cs = consumer_stats.snapshot().await;
        let error_counts = errors.take().await;

        ProducerConsumerResults {
            produced: BenchmarkSummary::from_snapshot_send_only(&ps, error_counts),
            consumed: BenchmarkSummary::from_snapshot_recv_only(&cs),
            producers: self.producers,
            consumers: self.consumers,
            target_rate: self.rate,
            duration: self.duration,
            csv_path: self.csv_path,
        }
    }
}

// ── Internal helpers ─────────────────────────────────────────────────────────

fn spawn_producer<PR: ProducerWork>(
    producer: PR,
    rate_ctrl: DynamicRateController,
    stats: Arc<Stats>,
    errors: ErrorCounter,
    running: Arc<AtomicBool>,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        let mut state = producer.init().await;
        while running.load(Ordering::Relaxed) {
            rate_ctrl.acquire().await;
            match producer.produce(&mut state).await {
                Ok(()) => {
                    stats.record_sent().await;
                }
                Err(reason) => {
                    tracing::debug!(reason, "produce error");
                    errors.record(&reason).await;
                    stats.record_error().await;
                }
            }
        }
        producer.cleanup(state).await;
    })
}

fn spawn_consumer<CO: ConsumerWork>(
    consumer: CO,
    stats: Arc<Stats>,
    running: Arc<AtomicBool>,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        let state = consumer.init().await;
        let recorder = crate::patterns::work::ConsumerRecorder::new(stats, running);
        consumer.run(state, recorder).await;
    })
}
