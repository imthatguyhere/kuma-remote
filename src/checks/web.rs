//! Web check: sends a single GET request to `url` and classifies the
//! result.
//!
//! When `test_string` is set, the body is always read and checked for it —
//! even on a non-2xx status, since the response still has a body worth
//! checking — but `Up` requires both a 2xx status and the match; either one
//! failing is `Down`, with the reason naming both the status and whether
//! the string matched.
//!
//! For an `https` `url`, a request that fails certificate validation is
//! retried once through `lenient_client` (which accepts invalid
//! certificates). If that retry succeeds, the check still proceeds as
//! reachable — a monitor shouldn't flip to `Down` purely because a cert
//! expired if the service itself is still answering — but a warning is
//! logged so the bad certificate doesn't go unnoticed.

use std::time::Instant;

use anyhow::{Context, Result};
use reqwest::Client;
use tracing::warn;

/// Outcome of running a single web check.
pub enum WebOutcome {
    /// Request succeeded and the response passed its up/down test (2xx
    /// status, plus a body match when `test_string` is set).
    /// `message` is the status code, plus whether `test_string` matched
    /// when one was configured, e.g. `200 ("Welcome" matched)`.
    Up { latency_ms: f64, message: String },
    /// Request failed outright, or the response failed its up/down test.
    Down { reason: String },
}

/// Request `url` once via `strict_client`, falling back to
/// `lenient_client` on an `https` certificate failure, and classify the
/// result:
///
/// - If `test_string` is `Some`, the body is always read and checked,
///   regardless of status. The check is `Up` iff the status is 2xx *and*
///   the body contains it; otherwise `Down`. Either way the message/reason
///   names both the status and whether it matched (e.g. `200 ("Welcome"
///   matched)` on `Up`, `404 ("Welcome" matched)` or `200 ("Welcome" not
///   matched)` on `Down`).
/// - If `test_string` is `None`, the check is `Up` iff the response status
///   is 2xx; otherwise `Down` with the status code as the reason.
pub async fn check_once(
    strict_client: &Client,
    lenient_client: &Client,
    check_id: &str,
    url: &str,
    test_string: Option<&str>,
) -> Result<WebOutcome> {
    let start = Instant::now();

    let response = match strict_client.get(url).send().await {
        Ok(response) => response,
        Err(strict_err) if url.starts_with("https://") => {
            match lenient_client.get(url).send().await {
                Ok(response) => {
                    warn!(
                        check_id = %check_id,
                        url = %url,
                        error = %strict_err,
                        "HTTPS request only succeeded with certificate validation \
                         disabled -- server is presenting an invalid, expired, or \
                         self-signed certificate"
                    );
                    response
                }
                Err(lenient_err) => {
                    return Ok(WebOutcome::Down {
                        reason: format!("Request failed: {lenient_err}"),
                    });
                }
            }
        }
        Err(err) => {
            return Ok(WebOutcome::Down {
                reason: format!("Request failed: {err}"),
            });
        }
    };

    let status = response.status();

    match test_string {
        Some(needle) => {
            let body = response
                .text()
                .await
                .with_context(|| format!("Reading response body from {url}"))?;
            let matched = body.contains(needle);
            let matched_str = if matched { "matched" } else { "not matched" };
            if status.is_success() && matched {
                Ok(WebOutcome::Up {
                    latency_ms: start.elapsed().as_secs_f64() * 1000.0,
                    message: format!("{status} (\"{needle}\" {matched_str})"),
                })
            } else {
                Ok(WebOutcome::Down {
                    reason: format!("{status} (\"{needle}\" {matched_str})"),
                })
            }
        }
        None => {
            if status.is_success() {
                Ok(WebOutcome::Up {
                    latency_ms: start.elapsed().as_secs_f64() * 1000.0,
                    message: format!("{status}"),
                })
            } else {
                Ok(WebOutcome::Down {
                    reason: format!("HTTP {status}"),
                })
            }
        }
    }
}
