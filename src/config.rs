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
    /// `User-Agent` header sent on both shared `reqwest::Client`s (the push
    /// client and the updater/web-check clients). Defaults to a desktop
    /// Chrome-on-Windows string rather than reqwest's own `reqwest/x.y.z`,
    /// since some reverse proxies / WAFs (e.g. Cloudflare bot protection)
    /// block generic HTTP-client user agents while allowing browsers. See
    /// `main.rs`.
    #[serde(default = "default_http_user_agent")]
    pub http_user_agent: String,
    /// Connection-establishment timeout for both shared `reqwest::Client`s
    /// (GitHub's API, an asset download, Kuma, or a `web`-checked site).
    /// Does not bound the release-asset download itself, which overrides
    /// this (see `updater.rs`). See `main.rs`.
    #[serde(with = "humantime_serde", default = "default_http_connect_timeout")]
    pub http_connect_timeout: Duration,
    /// Overall request/response timeout for both shared `reqwest::Client`s.
    /// Does not bound the release-asset download itself, which overrides
    /// this (see `updater.rs`). See `main.rs`.
    #[serde(with = "humantime_serde", default = "default_http_timeout")]
    pub http_timeout: Duration,
    /// Max response body size, in bytes, that a `web` check will buffer
    /// into memory. Enforced against `Content-Length` up front when present,
    /// and against bytes actually received as they stream in either way (in
    /// case `Content-Length` was absent or wrong) -- see `checks/web.rs`.
    /// Unrelated to the release-asset download's own `MAX_DOWNLOAD_SIZE` cap
    /// in `updater.rs`.
    #[serde(default = "default_web_max_response_size")]
    pub web_max_response_size: u64,
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

/// Default value for [`Config::http_user_agent`] when absent from the config file.
fn default_http_user_agent() -> String {
    "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 \
     (KHTML, like Gecko) Chrome/131.0.0.0 Safari/537.36"
        .to_string()
}

/// Default value for [`Config::http_connect_timeout`] when absent from the config file.
fn default_http_connect_timeout() -> Duration {
    Duration::from_secs(7)
}

/// Default value for [`Config::http_timeout`] when absent from the config file.
fn default_http_timeout() -> Duration {
    Duration::from_secs(30)
}

/// Default value for [`Config::web_max_response_size`] when absent from the config file.
fn default_web_max_response_size() -> u64 {
    25 * 1024 * 1024
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
    /// URL to request. Required (non-blank) for `Web`; unused otherwise. If
    /// it doesn't parse as an absolute URL (e.g. a bare `example.com` with
    /// no scheme), a warning is logged and `https://` is prepended by
    /// `Config::normalize` -- `reqwest` otherwise rejects a schemeless URL
    /// outright with a `builder error` instead of sending a request at all.
    #[serde(default)]
    pub url: Option<String>,
    /// Substring to search for in the response body. Only meaningful for
    /// `Web`. When set to a non-empty value, the body is always read and
    /// checked for it regardless of status, but `Up` requires both a 2xx
    /// status and the match; when unset or empty, the check reports
    /// `Up`/`Down` on the status code alone.
    #[serde(default)]
    pub test_string: Option<String>,
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
    /// Sends a GET request to `url` (required) and reports `Up`/`Down`
    /// based on the response. An `https` `url` whose certificate only
    /// validates with certificate checking disabled is still treated as
    /// reachable, but logs a warning about the bad certificate. See
    /// `checks::web` for the up/down classification rules.
    Web,
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
    /// interpretation, rather than forcing the user to hand-edit the file:
    /// a `push_url` with Kuma's example query string still attached, a
    /// schemeless `url` (assumed `https`), and a `test_string` set on a
    /// non-`web` check (warned about, not corrected, since there's no
    /// single obviously-intended fix).
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
            if check.test_string.is_some() && check.mode != CheckMode::Web {
                warn!(
                    check_id = %check.id,
                    mode = ?check.mode,
                    "test_string is set but mode is not web -- test_string is only \
                     used by mode web and will be ignored"
                );
            }
            if let Some(url) = check.url.as_mut()
                && url.trim().len() != url.len()
            {
                //=-- Trimmed first so a whitespace-only value (e.g. " ") becomes
                //=-- truly empty here, rather than surviving the emptiness check
                //=-- below and getting "https://" prepended into a bogus
                //=-- "https:// " that only fails later, at request time.
                *url = url.trim().to_string();
            }
            if let Some(url) = check
                .url
                .as_mut()
                .filter(|url| !url.is_empty() && reqwest::Url::parse(url).is_err())
            {
                warn!(
                    check_id = %check.id,
                    url = %url,
                    "url has no scheme -- assuming https and prepending it"
                );
                *url = format!("https://{url}");
            }
        }
    }

    /// Reject configs that are empty, have duplicate check ids, a
    /// zero-length interval (which would spin the scheduler tick forever), a
    /// zero `http_connect_timeout`/`http_timeout` (which would time out
    /// every connection/request instantly), a zero `web_max_response_size`
    /// (which would reject every web check's response body), an empty
    /// `http_user_agent` (which would send an empty User-Agent header,
    /// defeating the reason it's configurable in the first place), a `web`
    /// check with a missing or blank (empty or whitespace-only) `url`, or an
    /// `instance_lock_port` of `0` while the lock is actually in effect
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
        if self.http_connect_timeout.is_zero() {
            bail!("http_connect_timeout is zero, which would time out every connection instantly");
        }
        if self.http_timeout.is_zero() {
            bail!("http_timeout is zero, which would time out every request instantly");
        }
        if self.web_max_response_size == 0 {
            bail!("web_max_response_size is zero, which would reject every web check's response body");
        }
        if self.http_user_agent.is_empty() {
            bail!(
                "http_user_agent is empty, which sends an empty User-Agent header on every \
                 request -- the exact generic-client signature the default value exists to avoid"
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
            if check.mode == CheckMode::Web
                && check.url.as_deref().unwrap_or("").trim().is_empty()
            {
                bail!(
                    "Check {} uses mode web, which requires a non-empty url",
                    check.id
                );
            }
        }
        Ok(())
    }
}
