//! Logging initialization.
//!
//! Provides a simple wrapper around tracing-subscriber for consistent logging setup.

use crate::error::FrameworkError;

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
