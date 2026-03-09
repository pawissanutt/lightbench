//! Logging initialization.
//!
//! Provides a simple wrapper around tracing-subscriber for consistent logging setup.

use crate::error::FrameworkError;
use crate::time_sync::now_unix_ns_estimate;

/// Returns how many seconds have elapsed since the time system was first
/// initialised (same anchor used by [`now_unix_ns_estimate`]).  Starts at ~0.000.
pub fn elapsed_secs() -> f64 {
    // Touch the time-sync base so it is initialised on the very first call.
    // After that, compute elapsed from the same anchor used by benchmarks.
    static START_NS: std::sync::OnceLock<u64> = std::sync::OnceLock::new();
    let start = *START_NS.get_or_init(now_unix_ns_estimate);
    let now = now_unix_ns_estimate();
    now.saturating_sub(start) as f64 / 1_000_000_000.0
}

/// Initialize the logging/tracing system.
///
/// # Arguments
///
/// * `level` - Log level filter (e.g., "info", "debug", "trace", "warn", "error")
///
/// # Example
///
/// ```
/// use lightbench::logging;
///
/// logging::init("info").unwrap();
/// tracing::info!("Logging initialized");
/// ```
pub fn init(level: &str) -> Result<(), FrameworkError> {
    tracing_subscriber::fmt()
        .with_env_filter(level)
        .with_ansi(false)
        .try_init()
        .map_err(|e| FrameworkError::Logging(e.to_string()))?;
    Ok(())
}

/// Initialize logging with default settings (info level).
pub fn init_default() -> Result<(), FrameworkError> {
    init("info")
}
