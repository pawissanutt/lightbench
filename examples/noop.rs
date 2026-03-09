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
use std::time::Duration;

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
        .burst_factor(args.burst_factor)
        .progress(args.progress)
        .show_ramp_progress(args.show_ramp_progress);

    bench = match (args.rate, args.rate_per_worker) {
        (Some(r), _) => bench.rate(r),
        (_, Some(r)) => bench.rate_per_worker(r),
        _ => bench, // unlimited
    };

    if let Some(secs) = args.ramp_up {
        bench = bench.ramp_up(Duration::from_secs(secs));
        bench = bench.ramp_start_rate(args.ramp_start_rate);
    }

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
    ramp_up: Option<u64>,
    ramp_start_rate: f64,
    burst_factor: f64,
    csv: Option<String>,
    progress: bool,
    show_ramp_progress: bool,
}

fn parse_args() -> Args {
    let mut args = Args {
        rate: None,
        rate_per_worker: None,
        duration: 5,
        workers: 4,
        ramp_up: None,
        ramp_start_rate: 0.0,
        burst_factor: 0.1,
        csv: None,
        progress: true,
        show_ramp_progress: true,
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
            "--hide-ramp-progress" => args.show_ramp_progress = false,
            "--ramp-up" | "-u" => {
                args.ramp_up = iter.next().and_then(|v| v.parse().ok())
            }
            "--ramp-start" => {
                args.ramp_start_rate = iter
                    .next()
                    .and_then(|v| v.parse().ok())
                    .unwrap_or(args.ramp_start_rate)
            }
            "--burst-factor" => {
                args.burst_factor = iter
                    .next()
                    .and_then(|v| v.parse().ok())
                    .unwrap_or(args.burst_factor)
            }
            "--help" | "-h" => {
                println!(
                    "Usage: noop [OPTIONS]\n\
                     Options:\n  \
                       -r, --rate <N>            Total requests/sec (shared pool, <=0 for unlimited)\n  \
                       -R, --rate-per-worker <N> Requests/sec per worker (independent limiters)\n  \
                       -d, --duration <S>        Duration in seconds (default: 5)\n  \
                       -w, --workers <N>         Worker count (default: 4)\n  \
                       -u, --ramp-up <S>         Ramp-up duration in seconds (pre-measurement)\n  \
                           --ramp-start <N>      Initial rate at start of ramp (default: 0)\n  \
                           --burst-factor <F>    Burst allowance in seconds of tokens (default: 0.1)\n  \
                       --csv <FILE>              Write snapshots to CSV file\n  \
                       --no-progress             Disable progress display (output CSV to stdout)\n  \
                       --hide-ramp-progress      Hide progress output during ramp-up period"
                );
                std::process::exit(0);
            }
            _ => {}
        }
    }
    args
}
