//! Ping check: sends a single ICMP echo and classifies the result.
//!
//! Uses the `pinger` crate, which shells out to the OS `ping` command on
//! Unix and calls the native ICMP API (`IcmpSendEcho`) via `winping` on
//! Windows. Neither path requires elevated/administrator privileges.
//!
//! On Unix, `ping` streams echoes forever by default. Without a packet
//! limit, taking one result and dropping the channel would leave the
//! spawned `ping` process (and its reader thread) running indefinitely, so
//! Unix builds pass a packet count and wait-timeout flag to guarantee the
//! process exits on its own after a single probe. Windows sends one ICMP
//! request per call and has its own built-in 2s timeout, so no such flag
//! is needed there.

use std::time::Duration;

use anyhow::{Context, Result};
use pinger::{PingOptions, PingResult, ping};

/// Outcome of running a single ping against a host.
pub enum PingOutcome {
    /// Host replied within the timeout; latency in milliseconds.
    Up { latency_ms: f64 },
    /// Host did not reply, or the ping could not be completed.
    Down { reason: String },
}

/// Upper bound on how long we wait for a single ping result, regardless of
/// what the OS-level ping process is doing.
const RECV_TIMEOUT: Duration = Duration::from_secs(5);

/// Ping `host` once and classify the result. Runs on a blocking thread
/// since the underlying `pinger` API is synchronous.
pub async fn ping_once(host: String) -> Result<PingOutcome> {
    tokio::task::spawn_blocking(move || ping_once_blocking(&host))
        .await
        .context("Ping task panicked")?
}

/// Blocking implementation of [`ping_once`]: build OS-appropriate ping
/// options, send one probe, and wait up to [`RECV_TIMEOUT`] for a result.
fn ping_once_blocking(host: &str) -> Result<PingOutcome> {
    #[allow(unused_mut)]
    let mut options = PingOptions::new(host, Duration::from_secs(1), None);

    #[cfg(target_os = "linux")]
    {
        // -c 1: exit after one packet. -W 2: give up waiting after 2s if
        // the host never replies. Both flags are supported by iputils and
        // BusyBox ping alike.
        options = options.with_raw_arguments(vec!["-c", "1", "-W", "2"]);
    }
    #[cfg(any(
        target_os = "macos",
        target_os = "freebsd",
        target_os = "openbsd",
        target_os = "netbsd",
        target_os = "dragonfly"
    ))]
    {
        // BSD-family ping uses -t for the overall wait timeout, not TTL.
        options = options.with_raw_arguments(vec!["-c", "1", "-t", "2"]);
    }

    let receiver = ping(options).with_context(|| format!("Starting ping to {host}"))?;

    let outcome = match receiver.recv_timeout(RECV_TIMEOUT) {
        Ok(PingResult::Pong(duration, _)) => PingOutcome::Up {
            latency_ms: duration.as_secs_f64() * 1000.0,
        },
        Ok(PingResult::Timeout(_)) => PingOutcome::Down {
            reason: "Request timed out".to_string(),
        },
        Ok(PingResult::Unknown(line)) => PingOutcome::Down {
            reason: format!("Unrecognized ping output: {line}"),
        },
        Ok(PingResult::PingExited(status, stderr)) => PingOutcome::Down {
            reason: format!("Ping exited ({status}): {stderr}"),
        },
        Err(_) => PingOutcome::Down {
            reason: "No response within timeout".to_string(),
        },
    };

    Ok(outcome)
}
