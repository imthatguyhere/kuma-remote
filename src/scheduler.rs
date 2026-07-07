//! Per-check scheduling: each configured check runs independently on its
//! own interval, runs its check, and pushes the result to Uptime Kuma.

use reqwest::Client;
use tokio::task::JoinHandle;
use tokio::time::{MissedTickBehavior, interval};
use tracing::{error, info, warn};

use crate::checks::ping;
use crate::config::{CheckConfig, CheckMode};
use crate::kuma::{self, PushStatus};

/// Spawn one background task per check; returns their join handles so the
/// caller can abort them on shutdown. `debug` enables logging the exact
/// pushed URL (including query string) on every push.
pub fn spawn_all(checks: Vec<CheckConfig>, client: Client, debug: bool) -> Vec<JoinHandle<()>> {
    checks
        .into_iter()
        .map(|check| tokio::spawn(run_check_loop(check, client.clone(), debug)))
        .collect()
}

/// Tick `check`'s interval forever, running and pushing one result per
/// tick. Logs and swallows errors so one bad run doesn't kill the loop.
async fn run_check_loop(check: CheckConfig, client: Client, debug: bool) {
    let mut ticker = interval(check.interval);
    ticker.set_missed_tick_behavior(MissedTickBehavior::Delay);

    loop {
        ticker.tick().await;
        if let Err(err) = run_once(&check, &client, debug).await {
            error!(check_id = %check.id, error = %err, "Check run failed");
        }
    }
}

/// Run `check` once per its configured `mode`, log the outcome, and push
/// the resulting status to Kuma.
async fn run_once(check: &CheckConfig, client: &Client, debug: bool) -> anyhow::Result<()> {
    let status = match check.mode {
        CheckMode::Ping => match ping::ping_once(check.host.clone()).await? {
            ping::PingOutcome::Up { latency_ms } => {
                info!(check_id = %check.id, name = %check.name, latency_ms, "Up");
                PushStatus::Up {
                    ping_ms: Some(latency_ms),
                }
            }
            ping::PingOutcome::Down { reason } => {
                warn!(check_id = %check.id, name = %check.name, reason = %reason, "Down");
                PushStatus::Down { message: reason }
            }
        },
    };

    kuma::push(client, &check.push_url, status, debug).await
}
