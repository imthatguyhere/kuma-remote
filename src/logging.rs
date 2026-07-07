//! Structured logging setup for the daemon.

use tracing_subscriber::EnvFilter;

/// Initialize the global `tracing` subscriber. Honors `RUST_LOG` if set,
/// otherwise defaults to `info` level for all targets.
pub fn init() {
    tracing_subscriber::fmt()
        .with_env_filter(
            EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info")),
        )
        .init();
}
