//! Benchmark patterns for common workload types.
//!
//! - [`request`] - Simple request/response (fire-and-measure) pattern
//! - [`producer_consumer`] - Queue-based producer/consumer pattern
//! - [`async_task`] - Submit-and-poll async task pattern
//!
//! Worker lifecycle traits live in [`work`].

use crate::metrics::StatsSnapshot;
use crate::Stats;
use std::io::Write;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

pub mod async_task;
pub mod producer_consumer;
pub mod request;
pub mod work;

pub use async_task::AsyncTaskBenchmark;
pub use producer_consumer::ProducerConsumerBenchmark;
pub use request::Benchmark;
pub use work::{
    AsyncTaskResults, BenchmarkResults, BenchmarkSummary, BenchmarkWork, ConsumerWork, PollResult,
    PollWork, ProducerConsumerResults, ProducerWork, SubmitWork, WorkResult,
};

// ── Shared helpers for dual-stat patterns (producer/consumer, submit/poll) ──

pub(crate) type DualProgressFn =
    Arc<dyn Fn(&StatsSnapshot, &StatsSnapshot) -> Option<String> + Send + Sync + 'static>;

/// Spawn a snapshot task that reads two stat pools and writes CSV / progress.
///
/// While `in_ramp` is true, lines are prefixed with `[RAMP]` (or suppressed
/// entirely when `show_ramp_progress` is false).
pub(crate) fn spawn_dual_snapshot_task(
    primary_stats: Arc<Stats>,
    secondary_stats: Arc<Stats>,
    running: Arc<AtomicBool>,
    in_ramp: Arc<AtomicBool>,
    show_ramp_progress: bool,
    csv_path: Option<PathBuf>,
    show_progress: bool,
    progress_fn: DualProgressFn,
    csv_header: &str,
    csv_row_fn: fn(&StatsSnapshot, &StatsSnapshot) -> String,
) -> tokio::task::JoinHandle<()> {
    let header = csv_header.to_owned();
    tokio::spawn(async move {
        let mut ticker = tokio::time::interval(Duration::from_secs(1));
        let mut csv_writer = csv_path.as_ref().map(|p| {
            let f = std::fs::File::create(p).expect("Failed to create CSV file");
            std::io::BufWriter::new(f)
        });

        if let Some(w) = csv_writer.as_mut() {
            writeln!(w, "{}", header).ok();
        }
        if !show_progress {
            println!("{}", header);
        }

        while running.load(Ordering::Relaxed) {
            ticker.tick().await;
            let p = primary_stats.snapshot().await;
            let c = secondary_stats.snapshot().await;
            let row = csv_row_fn(&p, &c);
            let is_ramp = in_ramp.load(Ordering::Relaxed);

            if let Some(w) = csv_writer.as_mut() {
                writeln!(w, "{}", row).ok();
            }

            if is_ramp && !show_ramp_progress {
                continue;
            }

            if show_progress {
                if let Some(line) = progress_fn(&p, &c) {
                    if is_ramp {
                        crate::tprintln!("[RAMP] {}", line);
                    } else {
                        crate::tprintln!("{}", line);
                    }
                }
            } else {
                println!("{}", row);
            }
        }
    })
}
