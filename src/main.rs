//! Kuma Remote: a console client that runs local checks and reports their
//! results to Uptime Kuma push monitors. See `README.md` for configuration
//! and usage, and `FUNCTIONALITY.md` for internals.

mod checks;
mod config;
mod kuma;
mod logging;
mod scheduler;

use std::path::PathBuf;

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

/// Entry point: load config, spawn one scheduler task per check, then block
/// until Ctrl-C and abort all check tasks.
#[tokio::main]
async fn main() -> Result<()> {
    logging::init();

    let cli = Cli::parse();
    let config_path = resolve_config_path(cli.config);
    let config = config::Config::load(&config_path)
        .with_context(|| format!("Loading config from {}", config_path.display()))?;

    tracing::info!(
        checks = config.checks.len(),
        debug = config.debug,
        "Starting kuma-remote"
    );

    if config.debug {
        for check in &config.checks {
            tracing::info!(
                check_id = %check.id,
                name = %check.name,
                mode = ?check.mode,
                host = %check.host,
                push_url = %check.push_url,
                interval = ?check.interval,
                "Debug: check configured"
            );
        }
    }

    // Some reverse proxies / WAFs (e.g. Cloudflare bot protection) block
    // reqwest's default `reqwest/x.y.z` user agent while allowing browsers.
    // Presenting a normal desktop-browser user agent avoids that class of
    // false-positive block on the push URL.
    let client = reqwest::Client::builder()
        .user_agent(
            "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 \
             (KHTML, like Gecko) Chrome/131.0.0.0 Safari/537.36",
        )
        .build()
        .context("Building HTTP client")?;
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
