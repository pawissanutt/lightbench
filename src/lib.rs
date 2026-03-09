//! Lightbench - A transport-agnostic benchmarking framework.
//!
//! This crate provides foundational components for building high-performance
//! benchmarks measuring latency, throughput, and reliability metrics.
//!
//! # Features
//!
//! - **Runner**: High-level `Benchmark` builder with automatic rate distribution
//! - **Metrics**: HDR histogram-based latency tracking with atomic counters
//! - **Rate Control**: Token bucket rate limiter for open-loop benchmarks
//! - **Output**: Async CSV and stdout writers for metric snapshots
//!
//! # Quick Start
//!
//! ```rust,no_run
//! use lightbench::{Benchmark, WorkResult, now_unix_ns_estimate};
//! use lightbench::patterns::work::BenchmarkWork;
//!
//! #[derive(Clone)]
//! struct NoopWork;
//!
//! impl BenchmarkWork for NoopWork {
//!     type State = ();
//!     async fn init(&self) {}
//!     async fn work(&self, _: &mut ()) -> WorkResult {
//!         let start = now_unix_ns_estimate();
//!         // ... do work ...
//!         WorkResult::success(now_unix_ns_estimate() - start)
//!     }
//! }
//!
//! # tokio_test::block_on(async {
//! let results = Benchmark::new()
//!     .rate(1000.0)      // Total rate, auto-split across workers
//!     .workers(4)
//!     .duration_secs(10)
//!     .work(NoopWork)
//!     .run()
//!     .await;
//!
//! results.print_summary();
//! # });
//! ```
//!
//! # Modules
//!
//! - [`patterns`] - Benchmark patterns (request, producer/consumer, async task)
//! - [`metrics`] - Statistics collection with HDR histograms
//! - [`rate`] - Rate limiting for controlled benchmarks
//! - [`time_sync`] - Fast timestamp utilities
//! - [`output`] - CSV/stdout output writers
//! - [`logging`] - Tracing initialization

pub mod error;
pub mod logging;
pub mod metrics;
pub mod output;
pub mod patterns;
pub mod rate;
pub mod time_sync;

// Convenience re-exports
pub use error::FrameworkError;
pub use metrics::errors::ErrorCounter;
pub use metrics::{SequenceTracker, Stats, StatsSnapshot};
pub use output::OutputWriter;
pub use patterns::async_task::AsyncTaskBenchmark;
pub use patterns::producer_consumer::ProducerConsumerBenchmark;
pub use patterns::request::Benchmark;
pub use patterns::work::{
    AsyncTaskResults, BenchmarkResults, BenchmarkSummary, BenchmarkWork, ConsumerWork, PollResult,
    PollWork, ProducerConsumerResults, ProducerWork, SubmitWork, WorkResult,
};
pub use rate::{RateController, SharedRateController};
pub use time_sync::{latency_ns, now_unix_ns_estimate};

/// Like `println!` but prefixes every line with the elapsed time since the
/// program started (e.g. `[  1.234s]`), producing a scrolling log that the
/// user can review as history.
#[macro_export]
macro_rules! tprintln {
    () => { println!() };
    ($($arg:tt)*) => {
        println!("[{:>8.3}s] {}", $crate::logging::elapsed_secs(), format!($($arg)*))
    };
}
