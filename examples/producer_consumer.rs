//! Producer/Consumer Benchmark Example
//!
//! Benchmarks an in-memory queue with rate-controlled producers and
//! consumers that process items as fast as possible.
//!
//! The workload (a `VecDeque` queue) is defined here in the example.
//! The framework handles rate control, stats, progress, and CSV export.
//!
//! # Usage
//!
//! ```bash
//! cargo run --release --example producer_consumer --features clap -- \
//!     --producers 4 --consumers 4 --rate 10000 --duration 10
//! ```

use clap::Parser;
use lightbench::{
    logging, now_unix_ns_estimate, BenchmarkConfig, ConsumerWork, ProducerConsumerBenchmark,
    ProducerWork,
};
use std::collections::VecDeque;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::Mutex;

type Queue = Arc<Mutex<VecDeque<u64>>>;

// ---- CLI args --------------------------------------------------------------

#[derive(Parser)]
#[command(about = "In-memory producer/consumer benchmark")]
struct Args {
    /// Number of producer workers.
    #[arg(short = 'p', long, default_value = "1")]
    producers: usize,

    /// Number of consumer workers.
    #[arg(short = 'c', long, default_value = "1")]
    consumers: usize,

    #[command(flatten)]
    bench: BenchmarkConfig,
}

// ---- Producer --------------------------------------------------------------

/// Pushes the current timestamp into the shared queue.
#[derive(Clone)]
struct QueueProducer {
    queue: Queue,
}

impl ProducerWork for QueueProducer {
    type State = ();
    async fn init(&self) -> () {}
    async fn produce(&self, _: &mut ()) -> Result<(), String> {
        self.queue.lock().await.push_back(now_unix_ns_estimate());
        Ok(())
    }
}

// ---- Consumer --------------------------------------------------------------

/// Pops timestamps and returns queue-time latency in nanoseconds.
#[derive(Clone)]
struct QueueConsumer {
    queue: Queue,
}

impl ConsumerWork for QueueConsumer {
    type State = ();
    async fn init(&self) -> () {}
    async fn consume(&self, _: &mut ()) -> Option<u64> {
        self.queue
            .lock()
            .await
            .pop_front()
            .map(|ts| now_unix_ns_estimate().saturating_sub(ts))
    }
}

// ---- Main ------------------------------------------------------------------

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    logging::init("info").ok();

    let args = Args::parse();
    let bench_cfg = args.bench;

    // The workload: an in-memory queue shared between producers and consumers.
    let queue: Queue = Arc::new(Mutex::new(VecDeque::with_capacity(10_000)));

    let rate = bench_cfg.rate.unwrap_or(1000.0);
    let ramp_up = bench_cfg.ramp_up;
    let ramp_start_rate = bench_cfg.ramp_start_rate;
    let csv = bench_cfg.csv.clone();

    let mut bench = ProducerConsumerBenchmark::new()
        .producers(args.producers)
        .consumers(args.consumers)
        .rate(rate)
        .burst_factor(bench_cfg.burst_factor)
        .duration_secs(bench_cfg.duration)
        .progress(!bench_cfg.no_progress)
        .show_ramp_progress(!bench_cfg.hide_ramp_progress)
        .producer(QueueProducer {
            queue: queue.clone(),
        })
        .consumer(QueueConsumer {
            queue: queue.clone(),
        });

    if let Some(secs) = ramp_up {
        bench = bench.ramp_up(Duration::from_secs(secs));
        bench = bench.ramp_start_rate(ramp_start_rate);
    }

    if let Some(path) = csv {
        bench = bench.csv(path);
    }

    let results = bench.run().await;
    results.print_summary();
    Ok(())
}
