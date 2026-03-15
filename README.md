# Lightbench

A lightweight load testing framework for measuring latency, throughput, and reliability of external systems under sustained, rate-controlled load.

> **Load testing vs micro-benchmarking**: Lightbench is designed to drive load against
> external systems (HTTP services, message queues, async job APIs) — not to measure
> isolated code execution times. For micro-benchmarking Rust code, use
> [Criterion](https://github.com/bheisler/criterion.rs).

## Motivation

Built for academic research. Most load testers are great for DevOps — graphical UIs, rich dashboards, lots of bells and whistles — but they get in the way when you just need clean, reproducible numbers for a paper. The scriptable ones tend to be heavy enough that you start wondering how much of your measured latency is actually the tester's fault.

Rust fixes that. The overhead is tiny and predictable — no GC, no interpreter — so you can trust your numbers. And since a single Rust process can push very high request rates, you don't need a distributed load generation cluster just to stress-test one box.

## Features

- **Three Load Test Patterns**: Request, Producer/Consumer, and Async Task (submit + poll)
- **Load Test Runner**: High-level builder with automatic rate distribution across workers
- **Rate Control**: Per-worker token bucket (`RateController`) and shared lock-free pool (`SharedRateController`)
- **CSV Export**: Write snapshots to file with `.csv(path)` option
- **Progress Display**: User-friendly live progress or raw CSV output
- **HDR Histogram Metrics**: High-precision latency tracking with percentile reporting (p25, p50, p75, p95, p99)
- **Sequence Tracking**: Duplicate and gap detection for reliability measurement
- **Error Bucketing**: `ErrorCounter` groups errors by reason string for summary reporting

## Quick Start

Add to your `Cargo.toml`:

```toml
[dependencies]
lightbench = "0.3"
tokio = { version = "1", features = ["full"] }
```

To enable CLI argument parsing via [`clap`](https://docs.rs/clap), add the `clap` feature:

```toml
[dependencies]
lightbench = { version = "0.3", features = ["clap"] }
tokio = { version = "1", features = ["full"] }
clap = { version = "4", features = ["derive"] }
```

## Feature Flags

| Feature | Default | Description |
|---------|---------|-------------|
| `clap`  | off     | Derives `clap::Parser` on `BenchmarkConfig`, enabling direct CLI parsing with `BenchmarkConfig::parse()`. |

### `clap` Feature

When the `clap` feature is enabled, `BenchmarkConfig` derives `clap::Parser` and exposes all benchmark options (`--rate`, `--workers`, `--duration`, etc.) as CLI flags. You can either use it as a standalone parser or flatten it into a larger `clap` struct.

**Standalone parser** — simplest case, all options come from the command line:

```rust
use clap::Parser;
use lightbench::{Benchmark, BenchmarkConfig, BenchmarkWork, WorkResult};

#[derive(Clone)]
struct Noop;

impl BenchmarkWork for Noop {
    type State = ();
    async fn init(&self) -> () {}
    async fn work(&self, _: &mut ()) -> WorkResult { WorkResult::success(0) }
}

#[tokio::main]
async fn main() {
    let config = BenchmarkConfig::parse(); // reads --rate, --workers, --duration, etc.
    let results = Benchmark::from_config(config).work(Noop).run().await;
    results.print_summary();
}
```

Run it:

```bash
cargo run --release --features clap -- --rate 10000 --workers 4 --duration 10
```

**Embedded with `#[command(flatten)]`** — add your own flags alongside the benchmark options:

```rust
use clap::Parser;
use lightbench::{Benchmark, BenchmarkConfig, BenchmarkWork, WorkResult};

#[derive(Parser)]
struct Cli {
    #[command(flatten)]
    bench: BenchmarkConfig,

    /// URL to benchmark
    #[arg(long, default_value = "http://localhost:8080/")]
    url: String,
}

#[tokio::main]
async fn main() {
    let cli = Cli::parse();
    let results = Benchmark::from_config(cli.bench)
        .work(MyWork { url: cli.url })
        .run()
        .await;
    results.print_summary();
}
```

**Available CLI flags** (all optional, with defaults):

| Flag | Short | Default | Description |
|------|-------|---------|-------------|
| `--rate` | `-r` | unlimited | Total req/s shared across workers (`<=0` = unlimited) |
| `--rate-per-worker` | `-R` | — | Req/s per worker (ignored when `--rate` is set) |
| `--workers` | `-w` | `1` | Number of worker tasks |
| `--duration` | `-d` | `5` | Test duration in seconds |
| `--ramp-up` | `-u` | — | Ramp-up duration in seconds before the measured phase |
| `--ramp-start` | — | `0` | Initial rate at the start of ramp-up |
| `--burst-factor` | — | `0.1` | Burst allowance in seconds-worth of tokens |
| `--csv` | — | — | Write snapshots to a CSV file at this path |
| `--no-progress` | — | off | Disable live progress; write raw CSV rows to stdout |
| `--hide-ramp-progress` | — | off | Hide progress output during ramp-up |
| `--drain-timeout` | — | `30` | Seconds to wait for in-flight items after duration |

### Request Pattern (Request/Response)

```rust
use lightbench::{Benchmark, BenchmarkWork, WorkResult, now_unix_ns_estimate};

#[derive(Clone)]
struct MyWork { url: String }

struct MyState { client: reqwest::Client }

impl BenchmarkWork for MyWork {
    type State = MyState;

    async fn init(&self) -> MyState {
        // Called once per worker — put per-worker resources here.
        MyState { client: reqwest::Client::new() }
    }

    async fn work(&self, state: &mut MyState) -> WorkResult {
        let start = now_unix_ns_estimate();
        // ... your load test operation using state.client ...
        WorkResult::success(now_unix_ns_estimate() - start)
    }
}

#[tokio::main]
async fn main() {
    let results = Benchmark::new()
        .rate(1000.0)           // Total req/s (shared across workers)
        .workers(4)             // 4 workers compete for tokens
        .duration_secs(10)
        .csv("results.csv")     // Optional: export to CSV
        .progress(true)         // Optional: show progress (default: true)
        .work(MyWork { url: "http://localhost/".into() })
        .run()
        .await;

    results.print_summary();
}
```

**Worker lifecycle:** `init()` is called once per worker to create `State`. Put shared,
`Clone`-friendly resources (URLs, config, `Arc<Pool>`) in the struct. Put resources that
**must not be shared** across workers (HTTP clients, dedicated connections) in `State`.

**Rate Modes:**
- `.rate(1000.0)` — Shared rate pool (workers compete for 1000 total req/s)
- `.rate_per_worker(250.0)` — Each worker gets 250 req/s independently
- `.rate(0.0)` or `.rate(-1.0)` — Unlimited (maximum throughput)

### Producer/Consumer Pattern

```rust
use lightbench::{
    ProducerConsumerBenchmark, ProducerWork, ConsumerWork,
    ConsumerRecorder, now_unix_ns_estimate,
};
use std::collections::VecDeque;
use std::sync::Arc;
use tokio::sync::Mutex;

type Queue = Arc<Mutex<VecDeque<u64>>>;

#[derive(Clone)]
struct QueueProducer { queue: Queue }

impl ProducerWork for QueueProducer {
    type State = ();
    async fn init(&self) -> () {}
    async fn produce(&self, _: &mut ()) -> Result<(), String> {
        self.queue.lock().await.push_back(now_unix_ns_estimate());
        Ok(())
    }
}

#[derive(Clone)]
struct QueueConsumer { queue: Queue }

impl ConsumerWork for QueueConsumer {
    type State = ();
    async fn init(&self) -> () {}
    async fn run(&self, _state: (), recorder: ConsumerRecorder) {
        // Consumer owns its event loop — ideal for subscription-based APIs.
        while recorder.is_running() {
            let item = self.queue.lock().await.pop_front();
            match item {
                Some(ts) => {
                    recorder.record(now_unix_ns_estimate().saturating_sub(ts)).await;
                }
                None => tokio::task::yield_now().await,
            }
        }
    }
}

#[tokio::main]
async fn main() {
    let queue: Queue = Arc::new(Mutex::new(VecDeque::new()));

    let results = ProducerConsumerBenchmark::new()
        .producers(4)
        .consumers(4)
        .rate(10_000.0)         // Total produce rate (shared across producers)
        .duration_secs(10)
        .producer(QueueProducer { queue: queue.clone() })
        .consumer(QueueConsumer { queue: queue.clone() })
        .run()
        .await;

    results.print_summary();
}
```

**Trait contracts:**
- **ProducerWork::produce**: returns `Ok(())` on success or `Err(reason)` on failure. Rate-controlled by the framework.
- **ConsumerWork::run**: consumer owns its event loop. Use `recorder.record(latency_ns)` to report each consumed item and `recorder.is_running()` to check when to stop.

### Async Task Pattern (Submit + Poll)

```rust
use lightbench::{AsyncTaskBenchmark, PollResult};

#[tokio::main]
async fn main() {
    let results = AsyncTaskBenchmark::new()
        .submit_workers(4)
        .poll_workers(4)
        .rate(500.0)
        .duration_secs(10)
        .submit(|| Box::pin(async {
            // POST to API, return Some(task_id) or None on failure
            Some(submit_task().await)
        }))
        .poll(|task_id| Box::pin(async move {
            match check_task(task_id).await {
                TaskStatus::Done(latency_ns) => PollResult::Completed { latency_ns },
                TaskStatus::Running => PollResult::Pending,
                TaskStatus::Failed(e) => PollResult::Error(e),
            }
        }))
        .run()
        .await;

    results.print_summary();
}
```

## Examples

### Noop (framework overhead baseline)

```bash
cargo run --release --example noop
cargo run --release --example noop -- --rate 100000 --workers 8 --duration 5
```

Options: `--rate <N>`, `--rate-per-worker <N>`, `--workers <N>`, `--duration <S>`, `--csv <FILE>`, `--no-progress`

### HTTP GET Benchmark

```bash
cargo run --release --example http_get -- --rate 1000 --duration 10 --workers 4
```

Options:
- `--rate <N>` — Total requests/sec (shared pool, use `<=0` for unlimited)
- `--rate-per-worker <N>` — Requests/sec per worker (independent limiters)
- `--duration <S>` — Test duration in seconds (default: 10)
- `--workers <N>` — Worker count (default: 4)
- `--csv <FILE>` — Write snapshots to CSV
- `--no-progress` — Disable progress display, output CSV rows to stdout

### Producer/Consumer Benchmark

```bash
cargo run --release --example producer_consumer -- \
    --producers 4 --consumers 4 --rate 10000 --duration 10
```

Options: `--producers <N>`, `--consumers <N>`, `--rate <N>`, `--duration <S>`, `--csv <FILE>`, `--no-progress`

### Async Task Benchmark

```bash
cargo run --release --example async_task -- \
    --submit-workers 4 --poll-workers 4 --rate 500 --duration 10
```

Options: `--submit-workers <N>`, `--poll-workers <N>`, `--rate <N>`, `--duration <S>`, `--processing-delay <MS>`, `--csv <FILE>`, `--no-progress`

## Modules

### `patterns`

Three load test patterns, each a builder plus results type.

**`Benchmark`** (request pattern):
```rust
use lightbench::{Benchmark, BenchmarkWork, WorkResult, now_unix_ns_estimate};

#[derive(Clone)]
struct MyWork { url: String }
struct MyState { client: reqwest::Client }

impl BenchmarkWork for MyWork {
    type State = MyState;
    async fn init(&self) -> MyState { MyState { client: reqwest::Client::new() } }
    async fn work(&self, s: &mut MyState) -> WorkResult {
        let start = now_unix_ns_estimate();
        // ... use s.client to call the system under test ...
        WorkResult::success(now_unix_ns_estimate() - start)
    }
}

let results = Benchmark::new()
    .rate(1000.0)       // Shared rate pool (not split per-worker)
    .workers(4)         // Workers compete for tokens
    .duration_secs(10)
    .work(MyWork { url: "http://localhost/".into() })
    .run()
    .await;

results.print_summary();  // Formatted output
println!("Throughput: {:.2}", results.throughput());
println!("p99: {:.3}ms", results.p99_latency_ms());
```

**`ProducerConsumerBenchmark`**:
- `.producer(impl ProducerWork)` — rate-controlled, `produce()` returns `Ok(())` or `Err(reason)`
- `.consumer(impl ConsumerWork)` — consumer owns its event loop via `run(state, recorder)`, reports items with `recorder.record(latency_ns)`

**`AsyncTaskBenchmark`**:
- `.submit(fn)` — rate-controlled, returns `Some(task_id: u64)` or `None`
- `.poll(fn)` — free-running, returns `PollResult::{Completed{latency_ns}, Pending, Error(reason)}`

### `metrics`

Statistics collection with HDR histogram for latency tracking.

```rust
use lightbench::Stats;

let stats = Stats::new();
stats.record_sent().await;
stats.record_received(latency_ns).await;
stats.record_received_batch(&[lat1, lat2, lat3]).await; // Efficient batch

let snapshot = stats.snapshot().await;
println!("Throughput: {:.2}", snapshot.total_throughput());
println!("p99: {}ns", snapshot.latency_ns_p99);
```

**`SequenceTracker`** — per-consumer duplicate/gap detection:
```rust
use lightbench::SequenceTracker;

let mut tracker = SequenceTracker::new();
tracker.record(seq);          // returns false if duplicate
tracker.duplicate_count();
tracker.gap_count();          // gaps within min..=max range
tracker.head_loss();          // min_seq (sequences lost before first received)
```

**`ErrorCounter`** — thread-safe error bucketing:
```rust
use lightbench::ErrorCounter;

let counter = ErrorCounter::new();
counter.record("timeout").await;
counter.record("connection refused").await;
let errors = counter.take().await;  // HashMap<String, u64>
ErrorCounter::print_summary(&errors);
```

### `rate`

Token bucket rate limiters for controlled benchmarks.

**`RateController`** — per-worker:
```rust
use lightbench::RateController;

let mut rate = RateController::new(1000.0); // 1000 msg/s for this worker
loop {
    rate.wait_for_next().await;
    // send message...
}
```

**`SharedRateController`** — lock-free, shared across workers:
```rust
use lightbench::SharedRateController;
use std::sync::Arc;

let rate = Arc::new(SharedRateController::new(1000.0)); // 1000 msg/s total

for _ in 0..4 {
    let rate = rate.clone();
    tokio::spawn(async move {
        loop {
            rate.acquire().await;  // Workers compete for tokens
            // send message...
        }
    });
}
```

### `time_sync`

Fast timestamp utilities avoiding syscall overhead.

```rust
use lightbench::{now_unix_ns_estimate, latency_ns};

let start = now_unix_ns_estimate();
// ... do work ...
let elapsed = latency_ns(start);
```

### `logging`

Tracing initialization:

```rust
use lightbench::logging;

logging::init("info").ok();         // env-filter string
logging::init_default().ok();       // info level
```

### `output`

Async CSV and stdout writers:

```rust
use lightbench::output::OutputWriter;

let mut writer = OutputWriter::new_csv("results.csv".to_string()).await?;
writer.write_snapshot(&snapshot).await?;
writer.flush().await?;
```

## CSV Output Format

Snapshots are written as 19-column CSV rows:

```
timestamp,sent_count,received_count,error_count,total_throughput,interval_throughput,
latency_ns_p25,latency_ns_p50,latency_ns_p75,latency_ns_p95,latency_ns_p99,
latency_ns_min,latency_ns_max,latency_ns_mean,latency_ns_stddev,latency_sample_count,
duplicate_count,gap_count,head_loss
```

Quality columns (`duplicate_count`, `gap_count`, `head_loss`) are `0` unless a
`SequenceTracker` is in use.