//! Output writing for metrics snapshots.
//!
//! Provides async CSV and stdout writers for periodic metric snapshots.

use crate::error::FrameworkError;
use crate::metrics::StatsSnapshot;
use tokio::fs::{self, File};
use tokio::io::{AsyncWriteExt, BufWriter};

/// Output writer for metrics snapshots.
///
/// Supports CSV file and stdout output modes.
///
/// # Example
///
/// ```no_run
/// use lightbench::output::OutputWriter;
/// use lightbench::metrics::Stats;
///
/// # tokio_test::block_on(async {
/// let stats = Stats::new();
///
/// // CSV output
/// let mut writer = OutputWriter::new_csv("results.csv".into()).await.unwrap();
///
/// // Or stdout
/// let mut stdout_writer = OutputWriter::new_stdout();
///
/// let snapshot = stats.snapshot().await;
/// writer.write_snapshot(&snapshot).await.unwrap();
/// # });
/// ```
pub enum OutputWriter {
    /// CSV file writer with buffering.
    Csv(BufWriter<File>),
    /// Stdout writer.
    Stdout,
}

impl OutputWriter {
    /// Create a new CSV file writer.
    ///
    /// Creates parent directories if needed and writes the CSV header.
    pub async fn new_csv(path: String) -> Result<Self, FrameworkError> {
        // Ensure parent directory exists
        if let Some(parent) = std::path::Path::new(&path).parent() {
            if !parent.as_os_str().is_empty() {
                fs::create_dir_all(parent).await.ok();
            }
        }
        let file = File::create(&path).await?;
        let mut writer = BufWriter::new(file);

        // Write CSV header
        writer
            .write_all(StatsSnapshot::csv_header().as_bytes())
            .await?;
        writer.write_all(b"\n").await?;

        tracing::info!("Writing CSV output to: {}", path);
        Ok(Self::Csv(writer))
    }

    /// Create a stdout writer.
    pub fn new_stdout() -> Self {
        tracing::info!("Writing output to stdout");
        Self::Stdout
    }

    /// Write a statistics snapshot.
    ///
    /// For CSV mode, flushes after each write for tail-readable output.
    pub async fn write_snapshot(&mut self, snapshot: &StatsSnapshot) -> Result<(), FrameworkError> {
        match self {
            Self::Csv(writer) => {
                writer.write_all(snapshot.to_csv_row().as_bytes()).await?;
                writer.write_all(b"\n").await?;
                // Flush so external tail/readers see progress promptly
                writer.flush().await?;
            }
            Self::Stdout => {
                println!("{}", snapshot.to_csv_row());
            }
        }
        Ok(())
    }

    /// Flush any buffered output.
    pub async fn flush(&mut self) -> Result<(), FrameworkError> {
        if let Self::Csv(writer) = self {
            writer.flush().await?;
        }
        Ok(())
    }
}
