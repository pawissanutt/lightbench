//! Configuration structs for benchmark runners.
//!
//! [`BenchmarkConfig`] holds all common CLI-level options for the
//! [`Benchmark`](crate::patterns::request::Benchmark) runner.  When the
//! `clap` feature is enabled the struct derives [`clap::Parser`], so
//! callers can parse CLI arguments with a single `BenchmarkConfig::parse()`
//! call and then pass the result to
//! [`Benchmark::from_config`](crate::patterns::request::Benchmark::from_config).
//!
//! # Example (with `clap` feature)
//!
//! ```rust,no_run
//! # #[cfg(feature = "clap")]
//! # {
//! use lightbench::{Benchmark, BenchmarkConfig, BenchmarkWork, WorkResult};
//! use clap::Parser;
//!
//! #[derive(Clone)]
//! struct Noop;
//! impl BenchmarkWork for Noop {
//!     type State = ();
//!     async fn init(&self) -> () {}
//!     async fn work(&self, _: &mut ()) -> WorkResult { WorkResult::success(0) }
//! }
//!
//! # tokio_test::block_on(async {
//! let config = BenchmarkConfig::parse();
//! let results = Benchmark::from_config(config).work(Noop).run().await;
//! results.print_summary();
//! # });
//! # }
//! ```

/// Common benchmark configuration, usable as a standalone CLI parser or
/// embedded in a larger [`clap`] `Args` struct with `#[command(flatten)]`.
///
/// All fields have sensible defaults: 4 workers, 5-second duration,
/// unlimited rate.
#[derive(Debug, Clone)]
#[cfg_attr(feature = "clap", derive(clap::Parser))]
pub struct BenchmarkConfig {
    /// Total requests per second shared across all workers.
    ///
    /// Use a value `<= 0` or omit for unlimited rate.  Mutually exclusive
    /// with `--rate-per-worker`; if both are provided `--rate` wins.
    #[cfg_attr(feature = "clap", arg(short = 'r', long))]
    pub rate: Option<f64>,

    /// Requests per second per worker (each worker gets its own limiter).
    ///
    /// Ignored when `--rate` is also set.
    #[cfg_attr(feature = "clap", arg(short = 'R', long = "rate-per-worker"))]
    pub rate_per_worker: Option<f64>,

    /// Number of worker tasks (default: 4).
    #[cfg_attr(feature = "clap", arg(short = 'w', long, default_value = "4"))]
    pub workers: usize,

    /// Benchmark duration in seconds (default: 5).
    #[cfg_attr(feature = "clap", arg(short = 'd', long, default_value = "5"))]
    pub duration: u64,

    /// Ramp-up duration in seconds before the measured phase starts.
    ///
    /// During the ramp phase, the rate increases linearly from
    /// `--ramp-start` up to the target rate.
    #[cfg_attr(feature = "clap", arg(short = 'u', long = "ramp-up"))]
    pub ramp_up: Option<u64>,

    /// Initial rate (req/s) at the start of the ramp-up period (default: 0).
    ///
    /// Has no effect when `--ramp-up` is not set.
    #[cfg_attr(feature = "clap", arg(long = "ramp-start", default_value = "0"))]
    pub ramp_start_rate: f64,

    /// Burst allowance expressed as seconds-worth of accumulated tokens
    /// (default: 0.1).
    ///
    /// Higher values allow larger bursts when the system is briefly idle.
    #[cfg_attr(feature = "clap", arg(long = "burst-factor", default_value = "0.1"))]
    pub burst_factor: f64,

    /// Write per-interval snapshots to a CSV file at this path.
    #[cfg_attr(feature = "clap", arg(long))]
    pub csv: Option<String>,

    /// Disable live progress display.
    ///
    /// When set, raw CSV rows are written to stdout instead of the
    /// human-readable progress lines.
    #[cfg_attr(feature = "clap", arg(long = "no-progress"))]
    pub no_progress: bool,

    /// Hide progress output during the ramp-up phase.
    #[cfg_attr(feature = "clap", arg(long = "hide-ramp-progress"))]
    pub hide_ramp_progress: bool,

    /// Drain timeout in seconds: wait for in-flight items after the
    /// benchmark duration ends.  Set to `0` to disable draining.
    ///
    /// Applies to patterns with separate producer/consumer roles
    /// (e.g. `AsyncTaskBenchmark`, `ProducerConsumerBenchmark`).
    #[cfg_attr(feature = "clap", arg(long = "drain-timeout", default_value = "30"))]
    pub drain_timeout: u64,
}

impl Default for BenchmarkConfig {
    fn default() -> Self {
        Self {
            rate: None,
            rate_per_worker: None,
            workers: 4,
            duration: 5,
            ramp_up: None,
            ramp_start_rate: 0.0,
            burst_factor: 0.1,
            csv: None,
            no_progress: false,
            hide_ramp_progress: false,
            drain_timeout: 30,
        }
    }
}
