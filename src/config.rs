//! StrictYAML configuration: loading, schema, and validation.

use std::collections::HashSet;
use std::path::Path;
use std::time::Duration;

use anyhow::{Context, Result, bail};
use serde::Deserialize;
use tracing::warn;

/// Top-level config file layout: a flat list of checks.
#[derive(Debug, Clone, Deserialize)]
pub struct Config {
    /// When true, log every configured check at startup and the exact
    /// pushed URL (including query string) on every push. Off by default
    /// since a push URL is a bearer credential and shouldn't land in logs
    /// unless explicitly requested.
    #[serde(default)]
    pub debug: bool,
    /// When true, a check run that errors out (as opposed to completing and
    /// reporting `Down`, e.g. an unresolvable hostname) is also pushed to
    /// Kuma as a `down` status with the error as `msg`, instead of only
    /// being logged. On by default: without this, a run error sends no
    /// heartbeat at all, which leaves the Kuma monitor stuck showing its
    /// last known state (or pending) instead of reflecting the failure.
    #[serde(default = "default_report_run_failures")]
    pub report_run_failures: bool,
    /// When true (default), checks GitHub for a newer kuma-remote release at
    /// startup and replaces this binary in place before starting any checks.
    /// See `updater.rs`.
    #[serde(default = "default_auto_update")]
    pub auto_update: bool,
    /// When true, assumes a process supervisor (a Windows service manager, a
    /// systemd unit, ...) restarts kuma-remote on exit. An applied update
    /// then only replaces the binary on disk and exits — it does not spawn
    /// a replacement itself, and it never claims the single-instance lock
    /// (the supervisor is trusted to keep exactly one instance running).
    /// Off by default: without a supervisor, turning this on would leave
    /// the process stopped after an update until something else restarts
    /// it. See `updater.rs`.
    #[serde(default)]
    pub service_mode: bool,
    /// When true (default), and only when `service_mode` is false, an
    /// update-triggered restart — and startup in general — claims a
    /// single-instance lock before doing any real work, so a self-spawned
    /// replacement and a duplicate launch (accidental, or a supervisor
    /// restarting on top of one) never both end up running checks. Has no
    /// effect when `service_mode` is true, since that mode never claims the
    /// lock at all. See `updater.rs`.
    #[serde(default = "default_instance_lock")]
    pub instance_lock: bool,
    /// Loopback TCP port used by the single-instance lock (see
    /// `instance_lock`); nothing ever connects to it. Change this only if
    /// the default collides with something else on the host.
    #[serde(default = "default_instance_lock_port")]
    pub instance_lock_port: u16,
    /// When false (default), the update-asset download is capped at a fixed
    /// 5 minutes total, regardless of whether it's still making progress —
    /// intended to fail fast on a connection that's technically up but
    /// impractically slow. When true, that cap is lifted entirely and the
    /// download instead only aborts if it goes a full minute without
    /// receiving any data at all (a genuine stall, not just a slow
    /// connection). Turn this on for hosts on slow or unreliable links
    /// where a legitimate download might otherwise exceed 5 minutes; leave
    /// it off (and disable `auto_update` instead) on a host where you'd
    /// rather never risk running a partially-downloaded or virus-scan-stalled
    /// update. See `updater.rs`.
    #[serde(default)]
    pub slow_download_mode: bool,
    pub checks: Vec<CheckConfig>,
}

/// Default value for [`Config::report_run_failures`] when absent from the config file.
fn default_report_run_failures() -> bool {
    true
}

/// Default value for [`Config::auto_update`] when absent from the config file.
fn default_auto_update() -> bool {
    true
}

/// Default value for [`Config::instance_lock`] when absent from the config file.
fn default_instance_lock() -> bool {
    true
}

/// Default value for [`Config::instance_lock_port`] when absent from the config file.
fn default_instance_lock_port() -> u16 {
    51247
}

/// One monitored target and where to report its result.
#[derive(Debug, Clone, Deserialize)]
pub struct CheckConfig {
    /// Unique, human-chosen slug used in logs and for duplicate detection.
    pub id: String,
    /// Display name, for logs only.
    pub name: String,
    /// Check strategy to run against `host`.
    pub mode: CheckMode,
    /// IP address or hostname to check. Required for `Ping`; optional for
    /// `Heartbeat`, where it enables an additional reachability ping
    /// alongside the heartbeat.
    #[serde(default)]
    pub host: Option<String>,
    /// Full Uptime Kuma push URL, without a query string.
    pub push_url: String,
    /// How often to run this check, e.g. "60s", "5m", "1h".
    #[serde(with = "humantime_serde")]
    pub interval: Duration,
}

/// Check strategy. More modes extend this enum.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum CheckMode {
    /// Pings `host` (required) and reports `Up`/`Down` based on the reply.
    Ping,
    /// Always reports `Up` with message "Heartbeat", without checking
    /// anything. If `host` is given, also pings it and includes the
    /// latency; a failed ping does not turn the heartbeat `Down`.
    Heartbeat,
}

impl Config {
    /// Load, parse, and validate a StrictYAML config file from `path`.
    pub fn load(path: &Path) -> Result<Self> {
        let raw = std::fs::read_to_string(path)
            .with_context(|| format!("Reading config file {}", path.display()))?;
        let mut config: Config = strict_yaml_rust::serde::from_str(&raw)
            .with_context(|| format!("Parsing StrictYAML config {}", path.display()))?;
        config.normalize();
        config.validate()?;
        Ok(config)
    }

    /// Fix up known copy-paste mistakes that have one unambiguous correct
    /// interpretation, rather than forcing the user to hand-edit the file.
    fn normalize(&mut self) {
        for check in &mut self.checks {
            if let Some(query_start) = check.push_url.find('?') {
                warn!(
                    check_id = %check.id,
                    push_url = %check.push_url,
                    "Push_url contains a query string (likely copied from Kuma's \
                     `?status=up&msg=OK&ping=` dashboard example) — stripping it; \
                     kuma-remote adds its own status/msg/ping params"
                );
                check.push_url.truncate(query_start);
            }
        }
    }

    /// Reject configs that are empty, have duplicate check ids, a
    /// zero-length interval (which would spin the scheduler tick forever), or
    /// an `instance_lock_port` of `0` while the lock is actually in effect
    /// (port `0` always binds to a fresh OS-assigned port, so it can never
    /// detect a duplicate instance — silently defeating the lock instead of
    /// just weakening it, so this is a hard error rather than a warning).
    fn validate(&self) -> Result<()> {
        if self.checks.is_empty() {
            bail!("Config has no checks defined");
        }
        if self.instance_lock_port == 0 && self.instance_lock && !self.service_mode {
            bail!(
                "instance_lock_port is 0, which lets the OS assign a different port on \
                 every bind attempt, defeating the single-instance lock entirely. Set a \
                 fixed non-zero port, or disable the lock with instance_lock: false."
            );
        }
        let mut seen_ids = HashSet::new();
        for check in &self.checks {
            if !seen_ids.insert(check.id.as_str()) {
                bail!("Duplicate check id: {}", check.id);
            }
            if check.interval.is_zero() {
                bail!("Check {} has a zero interval", check.id);
            }
            if check.mode == CheckMode::Ping && check.host.is_none() {
                bail!("Check {} uses mode ping, which requires a host", check.id);
            }
        }
        Ok(())
    }
}
