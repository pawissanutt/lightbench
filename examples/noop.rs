//! Noop Benchmark — measures framework overhead.
//!
//! A no-op work function that returns instantly, showing the maximum
//! throughput the framework can sustain.
//!
//! # Usage
//!
//! ```bash
//! cargo run --release --example noop --features clap
//! cargo run --release --example noop --features clap -- --rate 100000 --workers 8 --duration 5
//! ```

use clap::Parser;
use lightbench::{logging, Benchmark, BenchmarkConfig, BenchmarkWork, WorkResult};

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
    let config = BenchmarkConfig::parse();
    let results = Benchmark::from_config(config).work(Noop).run().await;
    results.print_summary();
    Ok(())
}
