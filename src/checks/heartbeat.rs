//! Heartbeat check: always reports alive, optionally augmented with a ping.
//!
//! Unlike [`crate::checks::ping`], this check's purpose is to signal that
//! the `kuma-remote` process itself is running, not to test reachability of
//! `host`. So when `host` is given and the ping fails, the heartbeat still
//! reports `Up` -- just without a latency figure.

use tracing::warn;

use crate::checks::ping::{self, PingOutcome};

/// Result of running a heartbeat check: always `Up`, with an optional
/// latency when a `host` was configured and responded.
pub struct HeartbeatOutcome {
    pub latency_ms: Option<f64>,
}

/// Report a heartbeat. If `host` is `Some`, pings it once and includes the
/// latency on success; a missing host, or a failed/timed-out ping, simply
/// omits the latency rather than failing the check.
pub async fn beat_once(host: Option<String>) -> HeartbeatOutcome {
    let latency_ms = match host {
        Some(host) => {
            let host_display = host.clone();
            match ping::ping_once(host).await {
                Ok(PingOutcome::Up { latency_ms }) => Some(latency_ms),
                Ok(PingOutcome::Down { reason }) => {
                    warn!(host = %host_display, reason = %reason, "Heartbeat diagnostic ping failed");
                    None
                }
                Err(err) => {
                    warn!(host = %host_display, error = %err, "Heartbeat diagnostic ping errored");
                    None
                }
            }
        }
        None => None,
    };

    HeartbeatOutcome { latency_ms }
}
