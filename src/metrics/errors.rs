//! Error counting by reason for benchmark summaries.

use std::collections::HashMap;
use std::sync::Arc;
use tokio::sync::Mutex;

/// Thread-safe error counter that groups errors by reason string.
#[derive(Debug, Clone)]
pub struct ErrorCounter {
    counts: Arc<Mutex<HashMap<String, u64>>>,
}

impl ErrorCounter {
    /// Create a new error counter.
    pub fn new() -> Self {
        Self {
            counts: Arc::new(Mutex::new(HashMap::new())),
        }
    }

    /// Record an error with the given reason.
    pub async fn record(&self, reason: &str) {
        *self
            .counts
            .lock()
            .await
            .entry(reason.to_string())
            .or_insert(0) += 1;
    }

    /// Take the final error counts, consuming the accumulated data.
    pub async fn take(&self) -> HashMap<String, u64> {
        std::mem::take(&mut *self.counts.lock().await)
    }

    /// Print error breakdown to stdout.
    pub fn print_summary(errors: &HashMap<String, u64>) {
        if errors.is_empty() {
            return;
        }
        let mut sorted: Vec<_> = errors.iter().collect();
        sorted.sort_by(|a, b| b.1.cmp(a.1));
        println!("  Errors:");
        for (reason, count) in sorted {
            println!("    {}: {}", reason, count);
        }
    }
}

impl Default for ErrorCounter {
    fn default() -> Self {
        Self::new()
    }
}
