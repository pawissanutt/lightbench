//! HTTP GET Benchmark Example
//!
//! Demonstrates using lightbench's `Benchmark` runner for HTTP benchmarking.
//!
//! # Usage
//!
//! ```bash
//! cargo run --example http_get -- --rate 1000 --duration 10 --workers 4
//! ```

use lightbench::{logging, now_unix_ns_estimate, Benchmark, BenchmarkWork, WorkResult};
use std::net::SocketAddr;
use std::time::Duration;

use axum::{response::IntoResponse, routing::get, Router};

// ============================================================================
// Server (minimal echo server with timestamp)
// ============================================================================

async fn handle_get() -> impl IntoResponse {
    vec![0u8; 64]
}

async fn start_server(addr: SocketAddr) {
    let app = Router::new().route("/", get(handle_get));
    let listener = tokio::net::TcpListener::bind(addr).await.unwrap();
    tracing::info!("Server: http://{}", addr);
    axum::serve(listener, app).await.unwrap();
}

// ============================================================================
// Benchmark Work
// ============================================================================

#[derive(Clone)]
struct HttpWork {
    url: String,
}

struct HttpWorkerState {
    client: reqwest::Client,
}

impl BenchmarkWork for HttpWork {
    type State = HttpWorkerState;

    async fn init(&self) -> HttpWorkerState {
        let client = reqwest::Client::builder()
            .build()
            .expect("failed to build reqwest client");
        HttpWorkerState { client }
    }

    async fn work(&self, state: &mut HttpWorkerState) -> WorkResult {
        let start = now_unix_ns_estimate();
        match state.client.get(&self.url).send().await {
            Ok(r) if r.status().is_success() => WorkResult::success(now_unix_ns_estimate() - start),
            Ok(r) => WorkResult::error(format!("HTTP {}", r.status())),
            Err(e) => WorkResult::error(e.to_string()),
        }
    }
}

// ============================================================================
// Main
// ============================================================================

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    logging::init("info").ok();

    // Parse args
    let args = parse_args();
    let addr: SocketAddr = "127.0.0.1:8080".parse()?;
    let url = format!("http://{}/", addr);

    // Start server
    tokio::spawn(start_server(addr));
    tokio::time::sleep(Duration::from_millis(50)).await;

    // Build benchmark with appropriate rate mode
    let mut bench = Benchmark::new()
        .workers(args.workers)
        .duration_secs(args.duration)
        .burst_factor(args.burst_factor)
        .progress(args.progress)
        .show_ramp_progress(args.show_ramp_progress);

    bench = match (args.rate, args.rate_per_worker) {
        (Some(r), _) => bench.rate(r),
        (_, Some(r)) => bench.rate_per_worker(r),
        _ => bench, // unlimited (default)
    };

    if let Some(secs) = args.ramp_up {
        bench = bench.ramp_up(Duration::from_secs(secs));
        bench = bench.ramp_start_rate(args.ramp_start_rate);
    }

    if let Some(csv) = args.csv {
        bench = bench.csv(csv);
    }

    // Run benchmark
    let results = bench.work(HttpWork { url }).run().await;

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
        duration: 10,
        workers: 1,
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
                    "Usage: http_get [OPTIONS]\n\
                     Options:\n  \
                       -r, --rate <N>            Total requests/sec (shared pool, <=0 for unlimited)\n  \
                       -R, --rate-per-worker <N> Requests/sec per worker (independent limiters)\n  \
                       -d, --duration <S>        Duration in seconds (default: 10)\n  \
                       -w, --workers <N>         Worker count (default: 1)\n  \
                       -u, --ramp-up <S>         Ramp-up duration in seconds (pre-measurement)\n  \
                           --ramp-start <N>      Initial rate at start of ramp (default: 0)\n  \
                           --burst-factor <F>    Burst allowance in seconds of tokens (default: 0.1)\n  \
                       --csv <FILE>              Write snapshots to CSV file\n  \
                       --no-progress             Disable progress display (output CSV to stdout)\n\n\
                       --hide-ramp-progress      Hide progress output during ramp-up period\n\n\
                     Rate modes:\n  \
                       --rate 1000 --workers 4       => 1000 total (shared)\n  \
                       --rate-per-worker 250 -w 4   => 1000 total (250 each)\n  \
                       (no rate specified)           => unlimited"
                );
                std::process::exit(0);
            }
            _ => {}
        }
    }
    args
}
