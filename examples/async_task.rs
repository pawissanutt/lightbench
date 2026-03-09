//! Async Task Benchmark Example
//!
//! Benchmarks a submit-and-poll pattern:
//! 1. Submit workers POST tasks to an HTTP API
//! 2. Poll workers GET results until completed
//! 3. Framework tracks end-to-end task latency
//!
//! # Usage
//!
//! ```bash
//! cargo run --release --example async_task --features clap -- \
//!     --submit-workers 4 --poll-workers 4 --rate 500 --duration 10
//! ```

use clap::Parser;
use lightbench::{
    logging, now_unix_ns_estimate, AsyncTaskBenchmark, BenchmarkConfig, PollResult, PollWork,
    SubmitWork,
};
use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::RwLock;

use axum::{
    extract::{Path, State},
    http::StatusCode,
    response::IntoResponse,
    routing::{get, post},
    Json, Router,
};
use serde::{Deserialize, Serialize};

// ============================================================================
// Task Store (simulates backend job system)
// ============================================================================

#[derive(Clone)]
struct TaskInfo {
    submitted_ns: u64,
    completed_ns: Option<u64>,
}

struct TaskStore {
    tasks: RwLock<HashMap<u64, TaskInfo>>,
    next_id: AtomicU64,
    processing_delay_ms: u64,
}

impl TaskStore {
    fn new(processing_delay_ms: u64) -> Self {
        Self {
            tasks: RwLock::new(HashMap::new()),
            next_id: AtomicU64::new(1),
            processing_delay_ms,
        }
    }

    async fn submit(self: &Arc<Self>) -> u64 {
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        let info = TaskInfo {
            submitted_ns: now_unix_ns_estimate(),
            completed_ns: None,
        };
        self.tasks.write().await.insert(id, info);

        // Simulate async processing
        let delay = self.processing_delay_ms;
        let store = self.clone();
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(delay)).await;
            if let Some(task) = store.tasks.write().await.get_mut(&id) {
                task.completed_ns = Some(now_unix_ns_estimate());
            }
        });

        id
    }

    async fn poll(&self, id: u64) -> Option<(bool, u64)> {
        let tasks = self.tasks.read().await;
        tasks.get(&id).map(|t| {
            let completed = t.completed_ns.is_some();
            let latency = t
                .completed_ns
                .unwrap_or_else(now_unix_ns_estimate)
                .saturating_sub(t.submitted_ns);
            (completed, latency)
        })
    }
}

// ============================================================================
// HTTP Server
// ============================================================================

#[derive(Serialize, Deserialize)]
struct SubmitResponse {
    task_id: u64,
}

#[derive(Serialize, Deserialize)]
struct PollResponse {
    task_id: u64,
    completed: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    latency_ns: Option<u64>,
}

async fn handle_submit(State(store): State<Arc<TaskStore>>) -> impl IntoResponse {
    let task_id = store.submit().await;
    Json(SubmitResponse { task_id })
}

async fn handle_poll(
    State(store): State<Arc<TaskStore>>,
    Path(task_id): Path<u64>,
) -> impl IntoResponse {
    match store.poll(task_id).await {
        Some((completed, latency_ns)) => (
            StatusCode::OK,
            Json(PollResponse {
                task_id,
                completed,
                latency_ns: if completed { Some(latency_ns) } else { None },
            }),
        ),
        None => (
            StatusCode::NOT_FOUND,
            Json(PollResponse {
                task_id,
                completed: false,
                latency_ns: None,
            }),
        ),
    }
}

async fn start_server(addr: std::net::SocketAddr, store: Arc<TaskStore>) {
    let app = Router::new()
        .route("/submit", post(handle_submit))
        .route("/poll/{id}", get(handle_poll))
        .with_state(store);

    let listener = tokio::net::TcpListener::bind(addr).await.unwrap();
    axum::serve(listener, app).await.unwrap();
}

// ============================================================================
// Benchmark Work
// ============================================================================

#[derive(Clone)]
struct HttpSubmitter {
    client: reqwest::Client,
    url: String,
}

impl SubmitWork for HttpSubmitter {
    type State = ();
    async fn init(&self) -> () {}
    async fn submit(&self, _: &mut ()) -> Option<u64> {
        match self.client.post(&self.url).send().await {
            Ok(resp) if resp.status().is_success() => {
                resp.json::<SubmitResponse>().await.ok().map(|r| r.task_id)
            }
            _ => None,
        }
    }
}

#[derive(Clone)]
struct HttpPoller {
    client: reqwest::Client,
    base_url: String,
}

impl PollWork for HttpPoller {
    type State = ();
    async fn init(&self) -> () {}
    async fn poll(&self, task_id: u64, _: &mut ()) -> PollResult {
        let url = format!("{}/{}", self.base_url, task_id);
        match self.client.get(&url).send().await {
            Ok(resp) if resp.status().is_success() => match resp.json::<PollResponse>().await {
                Ok(body) if body.completed => PollResult::Completed {
                    latency_ns: body.latency_ns.unwrap_or(0),
                },
                Ok(_) => PollResult::Pending,
                Err(e) => PollResult::Error(e.to_string()),
            },
            _ => PollResult::Error("unknown task".into()),
        }
    }
}

// ============================================================================
// Main
// ============================================================================

// ---- CLI args --------------------------------------------------------------

#[derive(Parser)]
#[command(about = "Submit-and-poll async task benchmark")]
struct Args {
    /// Number of submit workers.
    #[arg(short = 's', long = "submit-workers", default_value = "2")]
    submit_workers: usize,

    /// Number of poll workers.
    #[arg(short = 'p', long = "poll-workers", default_value = "2")]
    poll_workers: usize,

    /// Simulated server-side processing delay in milliseconds.
    #[arg(short = 'D', long = "delay", default_value = "10")]
    processing_delay_ms: u64,

    #[command(flatten)]
    bench: BenchmarkConfig,
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    logging::init("info").ok();

    let args = Args::parse();
    let addr: std::net::SocketAddr = "127.0.0.1:8081".parse()?;

    // Start the server (system under test)
    let store = Arc::new(TaskStore::new(args.processing_delay_ms));
    tokio::spawn(start_server(addr, store.clone()));
    tokio::time::sleep(Duration::from_millis(50)).await;

    // Build HTTP client
    let client = reqwest::Client::builder()
        .pool_max_idle_per_host(args.submit_workers + args.poll_workers)
        .build()?;

    let bench_cfg = args.bench;
    let rate = bench_cfg.rate.unwrap_or(500.0);
    let ramp_up = bench_cfg.ramp_up;
    let ramp_start_rate = bench_cfg.ramp_start_rate;
    let csv = bench_cfg.csv.clone();

    // Build and run benchmark using the framework pattern
    let submit_url = format!("http://{}/submit", addr);
    let poll_url = format!("http://{}/poll", addr);

    let mut bench = AsyncTaskBenchmark::new()
        .submit_workers(args.submit_workers)
        .poll_workers(args.poll_workers)
        .rate(rate)
        .burst_factor(bench_cfg.burst_factor)
        .duration_secs(bench_cfg.duration)
        .progress(!bench_cfg.no_progress)
        .show_ramp_progress(!bench_cfg.hide_ramp_progress)
        .submitter(HttpSubmitter {
            client: client.clone(),
            url: submit_url,
        })
        .poller(HttpPoller {
            client,
            base_url: poll_url,
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
