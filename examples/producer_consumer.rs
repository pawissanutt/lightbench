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
//! cargo run --release --example producer_consumer -- \
//!     --producers 4 --consumers 4 --rate 10000 --duration 10
//! ```

use lightbench::{logging, now_unix_ns_estimate, ConsumerWork, ProducerConsumerBenchmark, ProducerWork};
use std::collections::VecDeque;
use std::sync::Arc;
use tokio::sync::Mutex;

type Queue = Arc<Mutex<VecDeque<u64>>>;

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

    let args = parse_args();

    // The workload: an in-memory queue shared between producers and consumers.
    let queue: Queue = Arc::new(Mutex::new(VecDeque::with_capacity(10_000)));

    let mut bench = ProducerConsumerBenchmark::new()
        .producers(args.producers)
        .consumers(args.consumers)
        .rate(args.rate)
        .duration_secs(args.duration)
        .progress(args.progress)
        .producer(QueueProducer { queue: queue.clone() })
        .consumer(QueueConsumer { queue: queue.clone() });

    if let Some(csv) = args.csv {
        bench = bench.csv(csv);
    }

    let results = bench.run().await;
    results.print_summary();
    Ok(())
}

struct Args {
    producers: usize,
    consumers: usize,
    rate: f64,
    duration: u64,
    csv: Option<String>,
    progress: bool,
}

fn parse_args() -> Args {
    let mut args = Args {
        producers: 1,
        consumers: 1,
        rate: 1000.0,
        duration: 10,
        csv: None,
        progress: true,
    };
    let mut iter = std::env::args().skip(1);

    while let Some(arg) = iter.next() {
        match arg.as_str() {
            "--producers" | "-p" => {
                args.producers = iter
                    .next()
                    .and_then(|v| v.parse().ok())
                    .unwrap_or(args.producers)
            }
            "--consumers" | "-c" => {
                args.consumers = iter
                    .next()
                    .and_then(|v| v.parse().ok())
                    .unwrap_or(args.consumers)
            }
            "--rate" | "-r" => {
                args.rate = iter
                    .next()
                    .and_then(|v| v.parse().ok())
                    .unwrap_or(args.rate)
            }
            "--duration" | "-d" => {
                args.duration = iter
                    .next()
                    .and_then(|v| v.parse().ok())
                    .unwrap_or(args.duration)
            }
            "--csv" => args.csv = iter.next(),
            "--no-progress" => args.progress = false,
            "--help" | "-h" => {
                println!(
                    "Usage: producer_consumer [OPTIONS]\n\
                     Options:\n  \
                       -p, --producers <N>  Number of producer workers (default: 1)\n  \
                       -c, --consumers <N>  Number of consumer workers (default: 1)\n  \
                       -r, --rate <N>       Total produce rate msg/s (default: 1000)\n  \
                       -d, --duration <S>   Duration in seconds (default: 10)\n  \
                       --csv <FILE>         Write snapshots to CSV file\n  \
                       --no-progress        Disable progress display"
                );
                std::process::exit(0);
            }
            _ => {}
        }
    }
    args
}
