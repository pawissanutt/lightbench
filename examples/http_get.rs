//! HTTP GET Benchmark Example
//!
//! Demonstrates using lightbench's `Benchmark` runner for HTTP benchmarking.
//!
//! # Usage
//!
//! ```bash
//! cargo run --example http_get --features clap -- --rate 1000 --duration 10 --workers 4
//! ```

use clap::Parser;
use lightbench::{logging, now_unix_ns_estimate, Benchmark, BenchmarkConfig, BenchmarkWork, WorkResult};
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

    let config = BenchmarkConfig::parse();
    let addr: SocketAddr = "127.0.0.1:8080".parse()?;
    let url = format!("http://{}/", addr);

    // Start server
    tokio::spawn(start_server(addr));
    tokio::time::sleep(Duration::from_millis(50)).await;

    // Run benchmark
    let results = Benchmark::from_config(config)
        .work(HttpWork { url })
        .run()
        .await;

    results.print_summary();
    Ok(())
}
