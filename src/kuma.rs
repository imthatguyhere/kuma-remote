//! Uptime Kuma push monitor client.
//!
//! Push monitors accept a GET request at `push_url` with `status`, `msg`,
//! and `ping` query parameters. See:
//! <https://github.com/louislam/uptime-kuma/wiki/Internal-API>

use anyhow::{Context, Result};
use reqwest::{Client, Url};
use tracing::info;

/// Kuma caps the `msg` field around 250 characters; longer messages are
/// truncated before sending so the push request is never rejected on that
/// basis alone.
const MAX_MESSAGE_LEN: usize = 250;

/// Outcome of a single check run, translated into Kuma's push vocabulary.
pub enum PushStatus {
    /// Host is reachable, or the check otherwise passed. `ping_ms` is
    /// omitted when latency isn't known. `message` defaults to `"OK"` when
    /// `None`.
    Up {
        ping_ms: Option<f64>,
        message: Option<String>,
    },
    /// Host is unreachable or the check otherwise failed.
    Down { message: String },
}

/// Push a check result to its configured Uptime Kuma push URL. When `debug`
/// is set, logs the exact pushed URL, including the query string -- off by
/// default since a push URL is itself a bearer credential.
pub async fn push(client: &Client, push_url: &str, status: PushStatus, debug: bool) -> Result<()> {
    let mut url = Url::parse(push_url).with_context(|| format!("Invalid push_url {push_url}"))?;

    {
        let mut query = url.query_pairs_mut();
        match status {
            PushStatus::Up { ping_ms, message } => {
                let message = message.as_deref().unwrap_or("OK");
                query
                    .append_pair("status", "up")
                    .append_pair("msg", message);
                if let Some(ping_ms) = ping_ms {
                    query.append_pair("ping", &format!("{ping_ms:.0}"));
                }
            }
            PushStatus::Down { message } => {
                let message = truncate(&message, MAX_MESSAGE_LEN);
                query
                    .append_pair("status", "down")
                    .append_pair("msg", &message);
            }
        }
    }

    if debug {
        info!(url = %url, "Debug: pushing to kuma");
    }

    let response = client
        .get(url.clone())
        .send()
        .await
        .with_context(|| format!("Sending push request to {url}"))?;

    if !response.status().is_success() {
        anyhow::bail!("Push to {url} returned HTTP {}", response.status());
    }
    Ok(())
}

/// Truncate `s` to at most `max_len` `char`s, cutting on a char boundary.
fn truncate(s: &str, max_len: usize) -> String {
    match s.char_indices().nth(max_len) {
        Some((byte_idx, _)) => s[..byte_idx].to_string(),
        None => s.to_string(),
    }
}
