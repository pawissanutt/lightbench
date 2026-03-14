//! Benchmark patterns for common workload types.
//!
//! - [`request`] - Simple request/response (fire-and-measure) pattern
//! - [`producer_consumer`] - Producer/consumer pattern (consumer owns its event loop)
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
    AsyncTaskResults, BenchmarkResults, BenchmarkSummary, BenchmarkWork, ConsumerRecorder,
    ConsumerWork, PollResult, PollWork, ProducerConsumerResults, ProducerWork, SubmitWork,
    WorkResult,
};

// ── Shared helpers for dual-stat patterns (producer/consumer, submit/poll) ──

pub(crate) type DualProgressFn =
    Arc<dyn Fn(&StatsSnapshot, &StatsSnapshot) -> Option<String> + Send + Sync + 'static>;

/// Display / CSV configuration for [`spawn_dual_snapshot_task`].
pub(crate) struct DualSnapshotConfig {
    pub in_ramp: Arc<AtomicBool>,
    pub show_ramp_progress: bool,
    pub csv_path: Option<PathBuf>,
    pub show_progress: bool,
    pub progress_fn: DualProgressFn,
    pub csv_header: &'static str,
    pub csv_row_fn: fn(&StatsSnapshot, &StatsSnapshot) -> String,
}

/// Spawn a snapshot task that reads two stat pools and writes CSV / progress.
///
/// While `in_ramp` is true, lines are prefixed with `[RAMP]` (or suppressed
/// entirely when `show_ramp_progress` is false).
pub(crate) fn spawn_dual_snapshot_task(
    primary_stats: Arc<Stats>,
    secondary_stats: Arc<Stats>,
    running: Arc<AtomicBool>,
    cfg: DualSnapshotConfig,
) -> tokio::task::JoinHandle<()> {
    let DualSnapshotConfig {
        in_ramp,
        show_ramp_progress,
        csv_path,
        show_progress,
        progress_fn,
        csv_header,
        csv_row_fn,
    } = cfg;
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

        while running.load(Ordering::Relaxed) {
            ticker.tick().await;
            let p = primary_stats.snapshot().await;
            let c = secondary_stats.snapshot().await;
            let row = csv_row_fn(&p, &c);
            let is_ramp = in_ramp.load(Ordering::Relaxed);

            if let Some(w) = csv_writer.as_mut() {
                writeln!(w, "{}", row).ok();
            }

            if !show_progress || (is_ramp && !show_ramp_progress) {
                continue;
            }

            if let Some(line) = progress_fn(&p, &c) {
                if is_ramp {
                    crate::tprintln!("[RAMP] {}", line);
                } else {
                    crate::tprintln!("{}", line);
                }
            }
        }
    })
}
