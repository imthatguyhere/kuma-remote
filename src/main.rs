//! Kuma Remote: a console client that runs local checks and reports their
//! results to Uptime Kuma push monitors. See `README.md` for configuration
//! and usage, and `FUNCTIONALITY.md` for internals.

mod checks;
mod config;
mod kuma;
mod logging;
mod scheduler;
mod updater;

use std::path::PathBuf;
use std::time::Duration;

use anyhow::{Context, Result};
use clap::Parser;

/// Kuma Remote -- push local check results to Uptime Kuma push monitors.
#[derive(Debug, Parser)]
#[command(version, about)]
struct Cli {
    /// Path to the StrictYAML config file listing checks to run. If not
    /// given, tries `kuma-remote.yaml`, `kuma-config.yaml`, then
    /// `config.yaml`, in that order, using the first one that exists.
    #[arg(short, long)]
    config: Option<PathBuf>,
}

/// Default config file names tried, in order, when `--config` is not given.
const DEFAULT_CONFIG_CANDIDATES: [&str; 3] =
    ["kuma-remote.yaml", "kuma-config.yaml", "config.yaml"];

/// Resolves the config path: the explicit `--config` value if given,
/// otherwise the first existing file among [`DEFAULT_CONFIG_CANDIDATES`],
/// falling back to the first candidate if none exist (so the subsequent
/// load error names a sensible default file).
fn resolve_config_path(explicit: Option<PathBuf>) -> PathBuf {
    if let Some(path) = explicit {
        return path;
    }
    DEFAULT_CONFIG_CANDIDATES
        .iter()
        .map(PathBuf::from)
        .find(|path| path.exists())
        .unwrap_or_else(|| PathBuf::from(DEFAULT_CONFIG_CANDIDATES[0]))
}

/// Entry point: log the app name/version/authors, load config, claim the
/// single-instance lock unless disabled (exiting immediately if another
/// instance already holds it, before any debug logging of check details),
/// log every check when `config.debug` is set, run the startup self-update
/// check (if enabled), spawn one scheduler task per check, then block until
/// Ctrl-C and abort all check tasks.
#[tokio::main]
async fn main() -> Result<()> {
    logging::init();

    tracing::info!(
        name = env!("CARGO_PKG_NAME"),
        version = env!("CARGO_PKG_VERSION"),
        authors = env!("CARGO_PKG_AUTHORS"),
        "kuma-remote starting"
    );

    let cli = Cli::parse();
    let config_path = resolve_config_path(cli.config);
    let config = config::Config::load(&config_path)
        .with_context(|| format!("Loading config from {}", config_path.display()))?;

    tracing::info!(
        checks = config.checks.len(),
        debug = config.debug,
        service_mode = config.service_mode,
        "Config loaded"
    );

    //=-- In service_mode, a process supervisor is trusted to guarantee a
    //=-- single running instance, so no lock is claimed at all. Otherwise,
    //=-- claim it unless explicitly disabled via `instance_lock: false` --
    //=-- claiming it here (not just around the updater) also guards against
    //=-- accidental double-launches in general. Runs on the blocking thread
    //=-- pool since claim_single_instance does blocking socket I/O and
    //=-- sleeps. See `updater.rs`. Done before the debug-logging block below
    //=-- so a duplicate instance exits without ever logging check details
    //=-- (including push_url, a bearer credential).
    let mut instance_lock = if config.service_mode || !config.instance_lock {
        None
    } else {
        let port = config.instance_lock_port;
        match tokio::task::spawn_blocking(move || updater::claim_single_instance(port))
            .await
            .context("Single-instance claim task panicked")?
        {
            updater::SingleInstance::AlreadyRunning => {
                tracing::warn!("Another kuma-remote instance is already running, exiting");
                return Ok(());
            }
            updater::SingleInstance::Claimed(listener) => Some(listener),
            updater::SingleInstance::Unavailable => None,
        }
    };

    if config.debug {
        for check in &config.checks {
            tracing::info!(
                check_id = %check.id,
                name = %check.name,
                mode = ?check.mode,
                host = ?check.host,
                push_url = %check.push_url,
                interval = ?check.interval,
                "Debug: check configured"
            );
        }
    }

    //=-- Some reverse proxies / WAFs (e.g. Cloudflare bot protection) block
    //=-- reqwest's default `reqwest/x.y.z` user agent while allowing browsers.
    //=-- Presenting a normal desktop-browser user agent avoids that class of
    //=-- false-positive block on the push URL. `connect_timeout` bounds how
    //=-- long establishing a connection can take (GitHub's API, an asset
    //=-- download, or Kuma itself); `timeout` additionally bounds the whole
    //=-- request/response cycle for everything except the release-asset
    //=-- download, which overrides it (see `updater.rs`) since that response
    //=-- can legitimately take longer than a small API/push response would.
    let client = reqwest::Client::builder()
        .user_agent(
            "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 \
             (KHTML, like Gecko) Chrome/131.0.0.0 Safari/537.36",
        )
        .connect_timeout(Duration::from_secs(7))
        .timeout(Duration::from_secs(30))
        .build()
        .context("Building HTTP client")?;

    if config.auto_update {
        updater::check_and_update(&client, config.service_mode, &mut instance_lock).await;
    }

    let handles = scheduler::spawn_all(
        config.checks,
        client,
        config.debug,
        config.report_run_failures,
    );

    tokio::signal::ctrl_c()
        .await
        .context("Waiting for ctrl-c")?;
    tracing::info!("Shutdown signal received, exiting");

    for handle in handles {
        handle.abort();
    }

    Ok(())
}
