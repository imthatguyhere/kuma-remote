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
    pub checks: Vec<CheckConfig>,
}

/// Default value for [`Config::report_run_failures`] when absent from the config file.
fn default_report_run_failures() -> bool {
    true
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
    /// IP address or hostname to check.
    pub host: String,
    /// Full Uptime Kuma push URL, without a query string.
    pub push_url: String,
    /// How often to run this check, e.g. "60s", "5m", "1h".
    #[serde(with = "humantime_serde")]
    pub interval: Duration,
}

/// Check strategy. Only `Ping` exists today; more modes extend this enum.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum CheckMode {
    Ping,
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
                     `?status=up&msg=OK&ping=` dashboard example) -- stripping it; \
                     kuma-remote adds its own status/msg/ping params"
                );
                check.push_url.truncate(query_start);
            }
        }
    }

    /// Reject configs that are empty, have duplicate check ids, or a
    /// zero-length interval (which would spin the scheduler tick forever).
    fn validate(&self) -> Result<()> {
        if self.checks.is_empty() {
            bail!("Config has no checks defined");
        }
        let mut seen_ids = HashSet::new();
        for check in &self.checks {
            if !seen_ids.insert(check.id.as_str()) {
                bail!("Duplicate check id: {}", check.id);
            }
            if check.interval.is_zero() {
                bail!("Check {} has a zero interval", check.id);
            }
        }
        Ok(())
    }
}
