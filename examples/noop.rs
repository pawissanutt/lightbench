//! Noop Benchmark — measures framework overhead.
//!
//! A no-op work function that returns instantly, showing the maximum
//! throughput the framework can sustain.
//!
//! # Usage
//!
//! ```bash
//! cargo run --release --example noop
//! cargo run --release --example noop -- --rate 100000 --workers 8 --duration 5
//! ```

use lightbench::{logging, Benchmark, BenchmarkWork, WorkResult};

#[derive(Clone)]
struct Noop;

impl BenchmarkWork for Noop {
    type State = ();
    async fn init(&self) -> () {}
    async fn work(&self, _: &mut ()) -> WorkResult {
        WorkResult::success(0)
    }
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    logging::init("info").ok();

    let args = parse_args();

    let mut bench = Benchmark::new()
        .workers(args.workers)
        .duration_secs(args.duration)
        .progress(args.progress);

    bench = match (args.rate, args.rate_per_worker) {
        (Some(r), _) => bench.rate(r),
        (_, Some(r)) => bench.rate_per_worker(r),
        _ => bench, // unlimited
    };

    if let Some(csv) = args.csv {
        bench = bench.csv(csv);
    }

    let results = bench.work(Noop).run().await;

    results.print_summary();
    Ok(())
}

struct Args {
    rate: Option<f64>,
    rate_per_worker: Option<f64>,
    duration: u64,
    workers: usize,
    csv: Option<String>,
    progress: bool,
}

fn parse_args() -> Args {
    let mut args = Args {
        rate: None,
        rate_per_worker: None,
        duration: 5,
        workers: 4,
        csv: None,
        progress: true,
    };
    let mut iter = std::env::args().skip(1);

    while let Some(arg) = iter.next() {
        match arg.as_str() {
            "--rate" | "-r" => args.rate = iter.next().and_then(|v| v.parse().ok()),
            "--rate-per-worker" | "-R" => {
                args.rate_per_worker = iter.next().and_then(|v| v.parse().ok())
            }
            "--duration" | "-d" => {
                args.duration = iter
                    .next()
                    .and_then(|v| v.parse().ok())
                    .unwrap_or(args.duration)
            }
            "--workers" | "-w" => {
                args.workers = iter
                    .next()
                    .and_then(|v| v.parse().ok())
                    .unwrap_or(args.workers)
            }
            "--csv" => args.csv = iter.next(),
            "--no-progress" => args.progress = false,
            "--help" | "-h" => {
                println!(
                    "Usage: noop [OPTIONS]\n\
                     Options:\n  \
                       -r, --rate <N>            Total requests/sec (shared pool, <=0 for unlimited)\n  \
                       -R, --rate-per-worker <N> Requests/sec per worker (independent limiters)\n  \
                       -d, --duration <S>        Duration in seconds (default: 5)\n  \
                       -w, --workers <N>         Worker count (default: 4)\n  \
                       --csv <FILE>              Write snapshots to CSV file\n  \
                       --no-progress             Disable progress display (output CSV to stdout)"
                );
                std::process::exit(0);
            }
            _ => {}
        }
    }
    args
}
