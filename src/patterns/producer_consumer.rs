//! Producer/Consumer benchmark pattern.
//!
//! Rate-controlled producers write items; free-running consumers read them.
//! Supply a [`ProducerWork`] and a [`ConsumerWork`] implementation; the
//! framework handles rate control, stats, progress, and CSV export.
//!
//! # Example
//!
//! ```ignore
//! use lightbench::{
//!     ProducerConsumerBenchmark, ProducerWork, ConsumerWork, now_unix_ns_estimate,
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
//!     async fn consume(&self, _: &mut ()) -> Option<u64> {
//!         self.queue.lock().await.pop_front()
//!             .map(|ts| now_unix_ns_estimate().saturating_sub(ts))
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

use crate::metrics::errors::ErrorCounter;
use crate::patterns::work::{BenchmarkSummary, ConsumerWork, ProducerConsumerResults, ProducerWork};
use crate::rate::RateController;
use crate::Stats;
use std::io::Write;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

/// Builder for producer/consumer benchmarks.
///
/// Producers are rate-controlled; consumers run as fast as possible
/// (returning `None` from [`ConsumerWork::consume`] yields the worker briefly).
///
/// Set both a [`ProducerWork`] and a [`ConsumerWork`] via [`.producer()`] and
/// [`.consumer()`] before calling [`.run()`].
pub struct ProducerConsumerBenchmark<PR = (), CO = ()> {
    producers: usize,
    consumers: usize,
    rate: f64,
    duration: Duration,
    csv_path: Option<PathBuf>,
    show_progress: bool,
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
            duration: Duration::from_secs(10),
            csv_path: None,
            show_progress: true,
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
    pub fn csv<P: Into<PathBuf>>(mut self, path: P) -> Self {
        self.csv_path = Some(path.into());
        self
    }

    /// Enable/disable progress display.
    pub fn progress(mut self, show: bool) -> Self {
        self.show_progress = show;
        self
    }

    /// Set the producer implementation.
    ///
    /// The framework clones `producer` once per producer worker. Put shared
    /// resources (queues, channels) in the struct; per-worker resources go
    /// in [`ProducerWork::State`] via [`ProducerWork::init`].
    pub fn producer<PR2: ProducerWork>(self, p: PR2) -> ProducerConsumerBenchmark<PR2, CO> {
        ProducerConsumerBenchmark {
            producers: self.producers,
            consumers: self.consumers,
            rate: self.rate,
            duration: self.duration,
            csv_path: self.csv_path,
            show_progress: self.show_progress,
            producer: Some(p),
            consumer: self.consumer,
        }
    }

    /// Set the consumer implementation.
    ///
    /// The framework clones `consumer` once per consumer worker.
    pub fn consumer<CO2: ConsumerWork>(self, c: CO2) -> ProducerConsumerBenchmark<PR, CO2> {
        ProducerConsumerBenchmark {
            producers: self.producers,
            consumers: self.consumers,
            rate: self.rate,
            duration: self.duration,
            csv_path: self.csv_path,
            show_progress: self.show_progress,
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

        tracing::info!(
            "ProducerConsumer: {} producers @ {:.0} msg/s, {} consumers, {}s",
            self.producers,
            self.rate,
            self.consumers,
            self.duration.as_secs()
        );

        let mut handles = Vec::new();

        // ---- Producers --------------------------------------------------
        let rate_per_producer = self.rate / self.producers as f64;
        for _ in 0..self.producers {
            let p = producer.clone();
            let stats = producer_stats.clone();
            let running = running.clone();
            let errors = errors.clone();
            handles.push(tokio::spawn(async move {
                let mut state = p.init().await;
                let mut rate_ctrl = RateController::new(rate_per_producer);
                while running.load(Ordering::Relaxed) {
                    rate_ctrl.wait_for_next().await;
                    match p.produce(&mut state).await {
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
                p.cleanup(state).await;
            }));
        }

        // ---- Consumers --------------------------------------------------
        for _ in 0..self.consumers {
            let c = consumer.clone();
            let stats = consumer_stats.clone();
            let running = running.clone();
            handles.push(tokio::spawn(async move {
                let mut state = c.init().await;
                while running.load(Ordering::Relaxed) {
                    match c.consume(&mut state).await {
                        Some(latency_ns) => {
                            stats.record_received(latency_ns).await;
                        }
                        None => {
                            tokio::task::yield_now().await;
                        }
                    }
                }
                c.cleanup(state).await;
            }));
        }

        // ---- Snapshot task ----------------------------------------------
        let ps = producer_stats.clone();
        let cs = consumer_stats.clone();
        let snap_running = running.clone();
        let csv_path = self.csv_path.clone();
        let show_progress = self.show_progress;
        let snapshot_handle = tokio::spawn(async move {
            let mut ticker = tokio::time::interval(Duration::from_secs(1));
            let mut csv_writer = csv_path.as_ref().map(|p| {
                let f = std::fs::File::create(p).expect("Failed to create CSV file");
                std::io::BufWriter::new(f)
            });

            let header =
                "timestamp,produced,consumed,in_flight,produce_rate,consume_rate,p50_ms,p95_ms,p99_ms";
            if let Some(w) = csv_writer.as_mut() {
                writeln!(w, "{}", header).ok();
            }
            if !show_progress {
                println!("{}", header);
            }

            while snap_running.load(Ordering::Relaxed) {
                ticker.tick().await;
                let p = ps.snapshot().await;
                let c = cs.snapshot().await;
                let in_flight = p.sent_count.saturating_sub(c.received_count);

                let row = format!(
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
                );

                if let Some(w) = csv_writer.as_mut() {
                    writeln!(w, "{}", row).ok();
                }

                if show_progress {
                    eprint!(
                        "\r  {} produced | {} consumed | {} in-flight | p50={:.2}ms  ",
                        p.sent_count,
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

        // ---- Wait and collect -------------------------------------------
        tokio::time::sleep(self.duration).await;
        running.store(false, Ordering::SeqCst);

        for handle in handles {
            let _ = handle.await;
        }
        let _ = snapshot_handle.await;

        let ps = producer_stats.snapshot().await;
        let cs = consumer_stats.snapshot().await;
        let error_counts = errors.take().await;
        let ns = |v: u64| v as f64 / 1_000_000.0;

        let produced = BenchmarkSummary {
            total_ops: ps.sent_count,
            successes: ps.sent_count,
            errors: ps.error_count,
            throughput: ps.total_throughput(),
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

        let consumed = BenchmarkSummary {
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

        ProducerConsumerResults {
            produced,
            consumed,
            producers: self.producers,
            consumers: self.consumers,
            target_rate: self.rate,
            duration: self.duration,
            csv_path: self.csv_path,
        }
    }
}
