# Lightbench Design Document

## Overview

Lightbench is a transport-agnostic **load testing** library. It drives sustained,
rate-controlled load against external systems — HTTP services, message queues, async
job APIs, or any other networked target — and measures their latency, throughput, and
reliability under that load. It ships with three ready-made load test patterns
(request, producer/consumer, async task) and a composable set of metrics, rate
control, and output primitives.

This is distinct from micro-benchmarking (e.g., Criterion), which measures the
execution time of isolated Rust code. Lightbench is about what happens to a *system*
when you send it 10 000 requests per second for 60 seconds.

## Architecture

```
┌─────────────────────────────────────────────────────────────────┐
│                        Application                               │
│  (HTTP load test, queue load test, custom load test)             │
└─────────────────────────────────────────────────────────────────┘
                              │
                              ▼
┌─────────────────────────────────────────────────────────────────┐
│                     Lightbench                              │
│ ┌─────────────┐ ┌─────────────┐ ┌─────────────┐ ┌─────────────┐ │
│ │   Metrics   │ │    Rate     │ │  Patterns   │ │   Output    │ │
│ │  (Stats,    │ │  (Token     │ │  (Request,  │ │  (CSV,      │ │
│ │   HDR,      │ │   Bucket,   │ │  Prod/Cons, │ │   Stdout)   │ │
│ │  Sequence,  │ │   Shared)   │ │  AsyncTask) │ │             │ │
│ │  Errors)    │ │             │ │             │ │             │ │
│ └─────────────┘ └─────────────┘ └─────────────┘ └─────────────┘ │
│ ┌─────────────┐ ┌─────────────┐                                  │
│ │  time_sync  │ │   logging   │                                  │
│ │  (fast ns)  │ │  (tracing)  │                                  │
│ └─────────────┘ └─────────────┘                                  │
└─────────────────────────────────────────────────────────────────┘
```

## Design Principles

### 1. Pattern-Based

The framework ships three load test patterns that encode common measurement workflows.
Users supply closures for the actual work (e.g., sending an HTTP request, pushing to a
queue); the framework handles rate control, worker management, progress display, and
CSV export.

### 2. Lock-Free Hot Paths

The `Stats` collector uses atomic counters for sent/received counts.
First-activity timestamps use `OnceLock<Instant>` — a bare atomic load after the first call.
Histogram contention is minimized by batching: each worker accumulates latencies in a
local `Vec<u64>` (capacity 64) and calls `record_received_batch()` when full, taking
one write lock per 64 records instead of one per request.
Single-record `record_received()` uses `write().await` for correctness.

### 3. Shared or Per-Worker Rate Control

`RateController` gives each worker an independent token bucket.
`SharedRateController` (lock-free atomic CAS) pools tokens across all workers so the
total rate matches the target exactly regardless of worker count.

## Component Details

### Patterns (`patterns/`)

Three high-level benchmark patterns, each a builder that accepts user-supplied closures.

**Benchmark** (`patterns/request.rs`) — request/response pattern
- `work(|| Box::pin(async { WorkResult::success(latency_ns) }))` closure per operation
- Workers share a `SharedRateController` (total mode) or each get a `RateController` (per-worker mode)
- Rate modes: `.rate(total)` (shared pool) or `.rate_per_worker(n)` (independent)
- Use `<=0` rate for unlimited throughput
- Returns `BenchmarkResults` with `print_summary()`, `throughput()`, `p99_latency_ms()`, etc.

**ProducerConsumerBenchmark** (`patterns/producer_consumer.rs`) — queue/pipeline pattern
- Producers: rate-controlled via `ProducerWork::produce()` returning `Ok(())` or `Err(reason)`
- Consumers: own their event loop via `ConsumerWork::run(state, recorder)` — suited for subscription-based APIs where messages are pushed to the consumer. The consumer reports each item with `recorder.record(latency_ns)` and exits when `recorder.is_running()` returns `false`.
- Returns `ProducerConsumerResults`

**AsyncTaskBenchmark** (`patterns/async_task.rs`) — submit-and-poll pattern
- Submit workers: rate-controlled closures returning `Some(task_id: u64)` on success or `None` on failure
- Poll workers: closures returning `PollResult::Completed { latency_ns }`, `PollResult::Pending`, or `PollResult::Error(reason)`
- Returns `AsyncTaskResults`

All three patterns share common builder methods: `.workers(n)`, `.duration_secs(n)`, `.csv(path)`, `.progress(bool)`.

### Metrics (`metrics/`)

**Stats**: Central statistics collector
- Atomic counters: `sent_count`, `received_count`, `error_count`
- HDR histogram for latency (1ns – 60s range, 3-digit precision)
- Connection tracking: `connections`, `active_connections`, `connection_attempts`, `connection_failures`
- Fault injection counters: `crashes_injected`, `reconnects`, `reconnect_failures`
- Quality metrics: `duplicate_count`, `gap_count`, `head_loss` (set by clients via `SequenceTracker`)
- Batch recording: `record_received_batch(&[u64])` takes a single histogram lock for multiple records
- Interval delta computation for throughput (tracks `last_sent_count`, `last_received_count`)
- First-activity timestamps via `OnceLock<Instant>` for accurate throughput (zero-cost after first call)

**StatsSnapshot**: Immutable point-in-time capture
- All counter values
- 5 latency percentiles (p25, p50, p75, p95, p99) plus min, max, mean, stddev
- Throughput: `total_throughput()` from first-activity time; `interval_throughput()` from last snapshot
- CSV serialization: 19-column format — core counters, throughputs, 10 latency fields, 3 quality metrics 
**SequenceTracker**: Per-consumer duplicate/gap detection
- `HashSet`-based seen tracking
- `record(seq)` → `bool` (true = new, false = duplicate)
- `record_batch(&[u64])` → `usize` (count of new sequences)
- `duplicate_count()`: received more than once
- `gap_count()`: missing sequences within min..=max received range
- `head_loss()`: `min_seq` value (sequences lost before first received, assuming seq starts at 0)
- No shared state—each consumer owns one

**ErrorCounter** (`metrics/errors.rs`): Thread-safe error bucketing
- Groups errors by string reason in an `Arc<Mutex<HashMap>>`
- `record(reason)` increments the count for that bucket
- `take()` drains the map for final reporting
- `print_summary(errors)` pretty-prints a sorted breakdown

### Rate Control (`rate.rs`)

**RateController**: Per-worker token bucket
- Tick rate: min(rate, 100) ticks/sec (≥10ms intervals) to amortize timer overhead
- Q24.8 fixed-point for fractional tokens
- Burst allowance: up to 10 ticks worth
- `MissedTickBehavior::Burst` for catch-up after scheduling delay
- Not `Clone` or `Sync`—one per worker task

**SharedRateController**: Lock-free shared token bucket
- Atomic CAS token management; safe to `Arc`-clone across worker tasks
- Refills tokens based on elapsed nanoseconds on each `acquire()` call
- 100ms burst allowance (`rate * 0.1` tokens max)
- `acquire()` yields to the Tokio runtime while spinning for tokens
- Use `rate <= 0` for unlimited mode (`acquire()` returns immediately)

### Time Sync (`time_sync.rs`)

Fast nanosecond timestamps:
- Caches `(Instant, unix_ns)` base on first call via `OnceLock`
- Subsequent calls: `base_unix_ns + instant.elapsed()`
- Avoids repeated `SystemTime::now()` syscalls
- `now_unix_ns_estimate() -> u64`: current UNIX time in nanoseconds
- `latency_ns(start_ns: u64) -> u64`: elapsed since a prior `now_unix_ns_estimate()` call

### Logging (`logging.rs`)

Thin wrapper around `tracing-subscriber`:
- `logging::init(level)` — initialize with a string filter (e.g., `"info"`, `"debug"`)
- `logging::init_default()` — initialize at `info` level
- Returns `FrameworkError::Logging` on double-init (safe to `.ok()` in examples)

### Output (`output.rs`)

**OutputWriter**: Async metric output
- `Csv(BufWriter<File>)`: creates parent directories, writes CSV header, flushes after each row for tail-readable output
- `Stdout`: `println`-based

Methods: `new_csv(path)`, `new_stdout()`, `write_snapshot(&StatsSnapshot)`, `flush()`

### Error (`error.rs`)

**FrameworkError**: thin `thiserror` enum
- `FrameworkError::Logging(String)`: tracing-subscriber init failure
- `FrameworkError::Io(std::io::Error)`: CSV/file I/O failure

## Extension Points

### Using the Patterns

The simplest extension is supplying a work implementation to the request pattern.
Shared resources (URLs, config) go in the struct; per-worker resources that must
not be shared (HTTP clients, connections) go in `State` and are created in `init()`:

```rust
use lightbench::{Benchmark, BenchmarkWork, WorkResult, now_unix_ns_estimate};

#[derive(Clone)]
struct MyWork { url: String }

struct MyState { client: reqwest::Client }

impl BenchmarkWork for MyWork {
    type State = MyState;

    async fn init(&self) -> MyState {
        MyState { client: reqwest::Client::new() } // one client per worker
    }

    async fn work(&self, state: &mut MyState) -> WorkResult {
        let start = now_unix_ns_estimate();
        // ... use state.client ...
        WorkResult::success(now_unix_ns_estimate() - start)
    }
}

Benchmark::new()
    .rate(1000.0)
    .workers(4)
    .duration_secs(10)
    .work(MyWork { url: "http://localhost/".into() })
    .run()
    .await
    .print_summary();
```

### Custom Output Formats

Wrap `StatsSnapshot` directly:

```rust
use lightbench::StatsSnapshot;

fn to_json(snapshot: &StatsSnapshot) -> String {
    serde_json::json!({
        "timestamp": snapshot.timestamp,
        "sent": snapshot.sent_count,
        "received": snapshot.received_count,
        "throughput": snapshot.total_throughput(),
        "latency": {
            "p50_ms": snapshot.latency_ns_p50 as f64 / 1_000_000.0,
            "p99_ms": snapshot.latency_ns_p99 as f64 / 1_000_000.0,
        }
    }).to_string()
}
```

### Custom Rate Patterns

Use `RateController` directly for variable-rate workloads:

```rust
use lightbench::RateController;
use std::time::Duration;

async fn variable_rate_benchmark() {
    let rates = vec![100.0, 500.0, 1000.0, 500.0, 100.0];

    for rate in rates {
        let mut controller = RateController::new(rate);
        let end = tokio::time::Instant::now() + Duration::from_secs(10);

        while tokio::time::Instant::now() < end {
            controller.wait_for_next().await;
            // send message...
        }
    }
}
```

## Performance Considerations

### Hot Path Optimization

1. **Sender**: `record_sent()` is atomic increment + `OnceLock::get_or_init` (bare atomic load after first call)
2. **Receiver (batch)**: workers buffer latencies locally in a `Vec<u64>` (cap 64), flushed via `record_received_batch()` — one histogram write lock per 64 records
3. **Receiver (single)**: `record_received()` uses `write().await` (always records, never drops)
4. **Rate (shared)**: `SharedRateController::acquire()` uses atomic CAS — no mutex on the hot path
5. **Per-worker state**: Use `BenchmarkWork::State` for resources that must not be shared (e.g. HTTP clients), created once per worker via `init()`

### Memory Usage

- `Stats`: ~1KB base + HDR histogram (~200KB for default precision)
- `SequenceTracker`: O(n) where n = unique sequences seen
- `ErrorCounter`: O(k) where k = unique error reasons

### Threading Model

- `Stats` is `Send + Sync`—share via `Arc<Stats>`
- `SharedRateController` is `Send + Sync`—share via `Arc<SharedRateController>`
- `RateController` is `Send` but not `Sync`—one per worker task
- `SequenceTracker` is single-threaded (no shared state; one per consumer)

## Future Considerations

1. **Prometheus Export**: Add optional prometheus metrics endpoint
2. **Percentile Customization**: Allow configuring which percentiles to track
3. **Windowed Stats**: Rolling window statistics (last N seconds)
4. **Binary Protocol**: Compact binary snapshot format for high-frequency export
