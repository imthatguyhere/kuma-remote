//! Web check: sends a single GET request to `url` and classifies the
//! result.
//!
//! When `test_string` is set, the body is always read and checked for it —
//! even on a non-2xx status, since the response still has a body worth
//! checking — but `Up` requires both a 2xx status and the match; either one
//! failing is `Down`, with the reason naming both the status and whether
//! the string matched.
//!
//! `latency_ms` always covers the whole page load (through the full body
//! download, not just headers) and reflects only the request that actually
//! answered: a failed `strict_client` attempt's time is never folded into a
//! successful `lenient_client` retry's latency.
//!
//! For an `https` `url`, a request that fails with a certificate error is
//! retried once through `lenient_client` (which accepts invalid
//! certificates). If that retry succeeds, the check still proceeds as
//! reachable — a monitor shouldn't flip to `Down` purely because a cert
//! expired if the service itself is still answering — but a warning is
//! logged so the bad certificate doesn't go unnoticed. Any other kind of
//! request failure (DNS, connection refused, timeout, ...) is reported
//! `Down` directly, without retrying.

use std::time::Instant;

use anyhow::Result;
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

/// True if `err` (or anything in its `source()` chain) looks like a TLS
/// certificate failure rather than some other connection problem. reqwest
/// has no dedicated certificate-error variant, so this matches on the
/// underlying TLS backend's error text -- the same fallback any caller of a
/// generic HTTP client has to use to tell a bad cert apart from, say, a
/// refused connection or a DNS failure.
fn is_certificate_error(err: &reqwest::Error) -> bool {
    let mut source: Option<&dyn std::error::Error> = Some(err);
    while let Some(err) = source {
        if err.to_string().to_lowercase().contains("certificate") {
            return true;
        }
        source = err.source();
    }
    false
}

/// Request `url` once via `strict_client`, falling back to `lenient_client`
/// on a certificate failure, and classify the result:
///
/// - If `test_string` is `Some`, the body is always read and checked,
///   regardless of status. The check is `Up` iff the status is 2xx *and*
///   the body contains it; otherwise `Down`. Either way the message/reason
///   names both the status and whether it matched (e.g. `200 ("Welcome"
///   matched)` on `Up`, `404 ("Welcome" matched)` or `200 ("Welcome" not
///   matched)` on `Down`).
/// - If `test_string` is `None`, the check is `Up` iff the response status
///   is 2xx; otherwise `Down` with the status code as the reason. The body
///   is still fully read either way, so `latency_ms` reflects the whole
///   page load rather than just time-to-first-byte.
pub async fn check_once(
    strict_client: &Client,
    lenient_client: &Client,
    check_id: &str,
    url: &str,
    test_string: Option<&str>,
) -> Result<WebOutcome> {
    let mut start = Instant::now();

    let response = match strict_client.get(url).send().await {
        Ok(response) => response,
        Err(strict_err) if is_certificate_error(&strict_err) => {
            //=-- Only the retry's own duration counts as latency -- the
            //=-- failed strict attempt's time is not this check's load time.
            start = Instant::now();
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

    //=-- Always drain the body so latency_ms reflects the whole page load,
    //=-- not just time-to-first-byte -- and so a mid-download failure is
    //=-- reported the same way as every other failure path here: Down with
    //=-- the error attached, not a hard error.
    let body = match response.text().await {
        Ok(body) => body,
        Err(err) => {
            return Ok(WebOutcome::Down {
                reason: format!("Reading response body: {err}"),
            });
        }
    };
    let latency_ms = start.elapsed().as_secs_f64() * 1000.0;

    match test_string {
        Some(needle) => {
            let matched = body.contains(needle);
            let matched_str = if matched { "matched" } else { "not matched" };
            if status.is_success() && matched {
                Ok(WebOutcome::Up {
                    latency_ms,
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
                    latency_ms,
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
