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
    /// Path to the StrictYAML config file listing checks to run.
    #[arg(short, long, default_value = "kuma-remote.yaml")]
    config: PathBuf,
}

/// Entry point: load config, spawn one scheduler task per check, then block
/// until Ctrl-C and abort all check tasks.
#[tokio::main]
async fn main() -> Result<()> {
    logging::init();

    let cli = Cli::parse();
    let config = config::Config::load(&cli.config)
        .with_context(|| format!("Loading config from {}", cli.config.display()))?;

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
