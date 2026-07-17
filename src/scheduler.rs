//! Per-check scheduling: each configured check runs independently on its
//! own interval, runs its check, and pushes the result to Uptime Kuma.

use reqwest::Client;
use tokio::task::JoinHandle;
use tokio::time::{MissedTickBehavior, interval};
use tracing::{error, info, warn};

use crate::checks::{heartbeat, ping, web};
use crate::config::{CheckConfig, CheckMode};
use crate::kuma::{self, PushStatus};

/// Spawn one background task per check; returns their join handles so the
/// caller can abort them on shutdown. `client` is used for every check
/// mode; `lenient_client` is only used by `Web` checks, as a fallback when
/// an `https` `url` fails certificate validation under `client`. `debug`
/// enables logging the exact pushed URL (including query string) on every
/// push. `report_run_failures` controls whether a run error (as opposed to
/// a completed `Down` result) is also pushed to Kuma as `down`, per
/// [`crate::config::Config::report_run_failures`].
pub fn spawn_all(
    checks: Vec<CheckConfig>,
    client: Client,
    lenient_client: Client,
    debug: bool,
    report_run_failures: bool,
) -> Vec<JoinHandle<()>> {
    checks
        .into_iter()
        .map(|check| {
            tokio::spawn(run_check_loop(
                check,
                client.clone(),
                lenient_client.clone(),
                debug,
                report_run_failures,
            ))
        })
        .collect()
}

/// Tick `check`'s interval forever, running and pushing one result per
/// tick. On a run error, logs it and, when `report_run_failures` is set,
/// also pushes it to Kuma as a `down` status so the failure is visible
/// there too. Swallows errors either way so one bad run doesn't kill the
/// loop.
async fn run_check_loop(
    check: CheckConfig,
    client: Client,
    lenient_client: Client,
    debug: bool,
    report_run_failures: bool,
) {
    let mut ticker = interval(check.interval);
    ticker.set_missed_tick_behavior(MissedTickBehavior::Delay);

    loop {
        ticker.tick().await;
        if let Err(err) = run_once(&check, &client, &lenient_client, debug).await {
            error!(check_id = %check.id, error = %err, "Check run failed");
            if report_run_failures {
                let status = PushStatus::Down {
                    message: format!("Check run failed: {err}"),
                };
                if let Err(push_err) = kuma::push(&client, &check.push_url, status, debug).await {
                    error!(check_id = %check.id, error = %push_err, "Failed to push run-failure status");
                }
            }
        }
    }
}

/// Run `check` once per its configured `mode`, log the outcome, and push
/// the resulting status to Kuma.
async fn run_once(
    check: &CheckConfig,
    client: &Client,
    lenient_client: &Client,
    debug: bool,
) -> anyhow::Result<()> {
    let status = match check.mode {
        CheckMode::Ping => {
            let host = check
                .host
                .as_ref()
                .expect("Config::validate requires a host for mode ping")
                .clone();
            match ping::ping_once(host).await? {
                ping::PingOutcome::Up { latency_ms } => {
                    info!(check_id = %check.id, name = %check.name, latency_ms, "Up");
                    PushStatus::Up {
                        ping_ms: Some(latency_ms),
                        message: None,
                    }
                }
                ping::PingOutcome::Down { reason } => {
                    warn!(check_id = %check.id, name = %check.name, reason = %reason, "Down");
                    PushStatus::Down { message: reason }
                }
            }
        }
        CheckMode::Heartbeat => {
            let heartbeat::HeartbeatOutcome { latency_ms } =
                heartbeat::beat_once(check.host.clone()).await;
            info!(check_id = %check.id, name = %check.name, latency_ms, "Heartbeat");
            PushStatus::Up {
                ping_ms: latency_ms,
                message: Some("Heartbeat".to_string()),
            }
        }
        CheckMode::Web => {
            let url = check
                .url
                .as_ref()
                .expect("Config::validate requires a url for mode web")
                .clone();
            match web::check_once(
                client,
                lenient_client,
                &check.id,
                &url,
                check.test_string.as_deref(),
            )
            .await?
            {
                web::WebOutcome::Up { latency_ms, message } => {
                    info!(check_id = %check.id, name = %check.name, latency_ms, message = %message, "Up");
                    PushStatus::Up {
                        ping_ms: Some(latency_ms),
                        message: Some(message),
                    }
                }
                web::WebOutcome::Down { reason } => {
                    warn!(check_id = %check.id, name = %check.name, reason = %reason, "Down");
                    PushStatus::Down { message: reason }
                }
            }
        }
    };

    kuma::push(client, &check.push_url, status, debug).await
}
