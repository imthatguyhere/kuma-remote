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

use anyhow::{Context, Result};
use clap::Parser;

/// Kuma Remote — push local check results to Uptime Kuma push monitors.
#[derive(Debug, Parser)]
#[command(version, about)]
struct Cli {
    /// Path to the StrictYAML config file listing checks to run. If not
    /// given, tries `kuma-remote.yaml`, `kuma-config.yaml`, then
    /// `config.yaml`, in that order, using the first one that exists.
    #[arg(short, long)]
    config: Option<PathBuf>,
    /// Ask a running instance to shut down gracefully via the
    /// single-instance lock port, then exit -- does not otherwise start
    /// kuma-remote. Requires `instance_lock: true` and `service_mode: false`
    /// in the config file (the same one a running instance would have
    /// used), since that's what determines the port to connect to.
    #[arg(long)]
    stop: bool,
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

/// Builds one of the two shared `reqwest::Client`s: the strict `client`
/// used everywhere, or (when `lenient` is set) the `lenient_client` used
/// only as a `web` check's certificate-failure fallback (see
/// `checks/web.rs`). Both share the same user agent and timeouts; `lenient`
/// is the only thing that differs between them.
fn build_http_client(config: &config::Config, lenient: bool) -> Result<reqwest::Client> {
    let builder = reqwest::Client::builder()
        .user_agent(config.http_user_agent.clone())
        .connect_timeout(config.http_connect_timeout)
        .timeout(config.http_timeout);
    let builder = if lenient {
        builder.danger_accept_invalid_certs(true)
    } else {
        builder
    };
    builder.build().with_context(|| {
        format!(
            "Building {} HTTP client",
            if lenient { "lenient" } else { "strict" }
        )
    })
}

/// Entry point: log the app name/version/authors, load config. If `--stop`
/// was given, hand off to `handle_stop` and return immediately -- nothing
/// below this point runs. Otherwise: claim the single-instance lock unless
/// disabled (exiting immediately if another instance already holds it,
/// before any debug logging of check details), log every check when
/// `config.debug` is set, run the startup self-update check (if enabled),
/// spawn one scheduler task per check, then block until either Ctrl-C or a
/// `--stop` request and abort all check tasks.
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

    if cli.stop {
        return handle_stop(&config).await;
    }

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
    let mut lock_unavailable = false;
    let (mut instance_lock, stop_requested) = if config.service_mode || !config.instance_lock {
        (None, None)
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
            updater::SingleInstance::Claimed {
                lock,
                stop_requested,
            } => (Some(lock), Some(stop_requested)),
            updater::SingleInstance::Unavailable => {
                lock_unavailable = true;
                (None, None)
            }
        }
    };

    if config.debug {
        for check in &config.checks {
            tracing::info!(
                check_id = %check.id,
                name = %check.name,
                mode = ?check.mode,
                host = ?check.host,
                url = ?check.url,
                test_string = ?check.test_string,
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
    //=-- All three are configurable (`http_user_agent`/`http_connect_timeout`/
    //=-- `http_timeout`) since a strict WAF/proxy or a slow link may need
    //=-- different values than the defaults below.
    let client = build_http_client(&config, false)?;

    //=-- Used only as a fallback by `web` checks, when an `https` `url`
    //=-- fails certificate validation under the strict `client` above --
    //=-- so a monitor doesn't flip to Down purely because a cert expired,
    //=-- as long as the server is still answering (see `checks/web.rs`).
    let lenient_client = build_http_client(&config, true)?;

    if config.auto_update {
        let outcome = updater::check_and_update(
            &client,
            config.service_mode,
            lock_unavailable,
            &mut instance_lock,
            config.slow_download_mode,
        )
        .await;
        if matches!(outcome, updater::UpdateOutcome::Exit) {
            //=-- An update was applied; this process's job is done. Returning
            //=-- here (rather than calling std::process::exit in updater.rs)
            //=-- keeps the exit on main's own return path, so any cleanup
            //=-- added here in the future isn't silently skipped by it.
            return Ok(());
        }
    }

    let handles = scheduler::spawn_all(
        config.checks,
        scheduler::HttpClients {
            client,
            lenient_client,
        },
        config.debug,
        config.report_run_failures,
    );

    //=-- Races Ctrl-C against a `--stop` request (see `updater.rs`) when the
    //=-- single-instance lock is held -- that's the only case where a
    //=-- `--stop` client has a listener to connect to in the first place, and
    //=-- also the only case where Ctrl-C can fail to reach this process (a
    //=-- self-spawned replacement is a detached grandchild the shell that
    //=-- launched the original process won't route Ctrl-C to anymore).
    match stop_requested {
        Some(stop_requested) => {
            tokio::select! {
                result = tokio::signal::ctrl_c() => {
                    result.context("Waiting for ctrl-c")?;
                    tracing::info!("Shutdown signal received (Ctrl-C), exiting");
                }
                () = stop_requested.notified() => {
                    tracing::info!("Shutdown signal received (--stop request), exiting");
                }
            }
        }
        None => {
            tokio::signal::ctrl_c()
                .await
                .context("Waiting for ctrl-c")?;
            tracing::info!("Shutdown signal received, exiting");
        }
    }

    for handle in handles {
        handle.abort();
    }

    Ok(())
}

/// Handles `--stop`: asks a running instance to shut down gracefully via
/// the single-instance lock port (see `updater::request_stop`). Does not
/// otherwise start kuma-remote.
async fn handle_stop(config: &config::Config) -> Result<()> {
    if config.service_mode {
        anyhow::bail!(
            "service_mode is true, so no single-instance lock/control channel is in use -- \
             stop kuma-remote through your process supervisor instead"
        );
    }
    if !config.instance_lock {
        anyhow::bail!(
            "instance_lock is false, so there is no control channel to connect to -- stop \
             kuma-remote via Task Manager or `Stop-Process` instead"
        );
    }
    if updater::request_stop(config.instance_lock_port)
        .await
        .context("Requesting stop")?
    {
        tracing::info!("Stop request acknowledged");
        Ok(())
    } else {
        anyhow::bail!(
            "No running kuma-remote instance found on port {} to stop",
            config.instance_lock_port
        )
    }
}
