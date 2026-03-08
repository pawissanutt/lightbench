//! Structured error types for the framework.

/// Framework errors.
#[derive(Debug, thiserror::Error)]
pub enum FrameworkError {
    /// Logging initialization failure.
    #[error("logging init failed: {0}")]
    Logging(String),

    /// I/O error (CSV write, file creation, etc.).
    #[error("io: {0}")]
    Io(#[from] std::io::Error),
}
