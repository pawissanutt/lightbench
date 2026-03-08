//! Benchmark patterns for common workload types.
//!
//! - [`request`] - Simple request/response (fire-and-measure) pattern
//! - [`producer_consumer`] - Queue-based producer/consumer pattern
//! - [`async_task`] - Submit-and-poll async task pattern
//!
//! Worker lifecycle traits live in [`work`].

pub mod async_task;
pub mod producer_consumer;
pub mod request;
pub mod work;

pub use async_task::AsyncTaskBenchmark;
pub use producer_consumer::ProducerConsumerBenchmark;
pub use request::Benchmark;
pub use work::{
    AsyncTaskResults, BenchmarkResults, BenchmarkSummary, BenchmarkWork, ConsumerWork,
    PollResult, PollWork, ProducerConsumerResults, ProducerWork, SubmitWork, WorkResult,
};
