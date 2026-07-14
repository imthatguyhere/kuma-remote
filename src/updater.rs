//! Startup self-updater: checks GitHub's latest release of this repo for a
//! release asset matching the currently running executable's file name, and
//! compares its GitHub-computed SHA-256 digest against the running exe's own
//! hash. On a mismatch, downloads the asset and verifies its hash, then
//! either (default) replaces the running exe in place, spawns a replacement
//! process, and exits, or (`service_mode`) just replaces it and exits,
//! trusting a process supervisor to restart it.
//!
//! Spawning a replacement ourselves (the default) is what makes an update
//! take effect immediately even when kuma-remote is run bare, with nothing
//! supervising it at all -- which must keep working, since that's a normal
//! way to run this tool. The risk that creates -- a supervisor that *also*
//! restarts on exit (NSSM's default `AppExit` behavior, or a systemd unit
//! with `Restart=always`) ending up running both the self-spawned
//! replacement and its own fresh instance, permanently, after every update
//! -- is closed by [`SingleInstance`]: every process, whether spawned by us
//! or by a supervisor, must claim a fixed loopback port before doing any
//! real work. Only one process can ever hold it; whichever one loses just
//! exits immediately. Since the exe on disk is already the new version by
//! the time anyone is racing for the port, it doesn't matter which one wins
//! -- exactly one instance of the new version ends up running either way.
//!
//! Self-spawning a replacement is only safe when this process actually holds
//! the single-instance lock (a live [`SingleInstance::Claimed`]): that's what
//! guarantees a losing duplicate exits instead of piling up. If the lock
//! isn't held -- `instance_lock: false`, or a claim that came back
//! [`SingleInstance::Unavailable`] -- self-spawning would be unprotected, so
//! `try_update` falls back to the plain exit-only path in that case too, not
//! just under `service_mode`.
//!
//! `service_mode` turns both off unconditionally: no self-spawned replacement
//! (the supervisor is trusted to restart on exit), and no single-instance
//! lock is ever claimed in the first place (see `main.rs`). It exists for
//! deployments that already have a supervisor and would rather it own the
//! restart, full stop.
//!
//! One consequence of the single-instance lock when it *is* in use: only
//! one kuma-remote instance can run per machine at a time. If you need two
//! independent sets of checks, list them all in one config file rather than
//! running two instances.
//!
//! Every failure mode here (network, rate limiting, missing digest, no
//! matching asset, permissions, ...) is logged and swallowed rather than
//! propagated -- a failed or skipped update check must never prevent
//! kuma-remote from starting its configured checks.

use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::time::Duration;

use anyhow::{Context, Result};
use reqwest::Client;
use serde::Deserialize;
use sha2::{Digest, Sha256};
use tracing::{error, info, warn};

//=-- Retry budget for a port bind that fails with `AddrInUse`, before
//=-- concluding another instance is genuinely running. Covers the brief
//=-- window between an updater spawning its replacement and that
//=-- replacement claiming the port for itself.
const CLAIM_ATTEMPTS: u32 = 10;
const CLAIM_RETRY_DELAY: Duration = Duration::from_millis(100);

//=-- How long to wait for the identity handshake (see `LOCK_MAGIC`) when
//=-- another process already holds the lock port.
const HANDSHAKE_TIMEOUT: Duration = Duration::from_millis(200);

//=-- Bytes the lock holder writes back to any connection on the lock port,
//=-- so a challenger that loses the bind race can tell "another kuma-remote
//=-- instance holds this" apart from "some unrelated process/service
//=-- happens to be bound to this port".
const LOCK_MAGIC: &[u8] = b"kuma-remote-single-instance-v1";

//=-- How many times, and how far apart, `try_update` retries spawning the
//=-- replacement process before giving up and logging it as stuck.
const SPAWN_ATTEMPTS: u32 = 3;
const SPAWN_RETRY_DELAY: Duration = Duration::from_millis(250);

/// Outcome of [`claim_single_instance`].
pub enum SingleInstance {
    /// This process holds the lock. Keep the listener alive for as long as
    /// this process should count as "the" running instance; dropping it
    /// (including via [`Option::take`]) releases the claim immediately.
    Claimed(TcpListener),
    /// Another instance already holds the lock; this process must not do
    /// any real work.
    AlreadyRunning,
    /// The lock could not be claimed for a reason unrelated to another
    /// instance running (e.g. a local permissions/network-stack issue, or
    /// the port being held by an unrelated process). Treated as "proceed
    /// anyway, without the guarantee" -- this safety net must never itself
    /// block kuma-remote from starting.
    Unavailable,
}

/// Tries to claim `port` (see `Config::instance_lock_port`) as a
/// cross-process single-instance mutex. Retries briefly on `AddrInUse` (see
/// [`CLAIM_ATTEMPTS`]) before concluding another instance is running. Runs
/// blocking I/O throughout -- callers on a tokio runtime should invoke this
/// via `tokio::task::spawn_blocking` rather than calling it directly from an
/// async context.
pub fn claim_single_instance(port: u16) -> SingleInstance {
    for attempt in 0..CLAIM_ATTEMPTS {
        match TcpListener::bind(("127.0.0.1", port)) {
            Ok(listener) => {
                spawn_handshake_responder(&listener);
                return SingleInstance::Claimed(listener);
            }
            Err(err) if err.kind() == std::io::ErrorKind::AddrInUse => {
                if attempt + 1 == CLAIM_ATTEMPTS {
                    return classify_occupant(port);
                }
                std::thread::sleep(CLAIM_RETRY_DELAY);
            }
            Err(err) => {
                warn!(error = %err, "Could not claim single-instance lock, proceeding anyway");
                return SingleInstance::Unavailable;
            }
        }
    }
    classify_occupant(port)
}

//=-- Spawns a detached background thread that answers every connection on
//=-- `listener` with `LOCK_MAGIC`, so a challenger can confirm this is
//=-- genuinely a kuma-remote instance holding the port. Runs for as long as
//=-- `listener` (or its clone) stays open; best-effort, so a failure to
//=-- clone just means a challenger later treats this occupant as
//=-- unidentifiable rather than confirmed.
fn spawn_handshake_responder(listener: &TcpListener) {
    let responder = match listener.try_clone() {
        Ok(responder) => responder,
        Err(err) => {
            warn!(error = %err, "Could not start single-instance handshake responder");
            return;
        }
    };
    std::thread::spawn(move || {
        for mut stream in responder.incoming().flatten() {
            let _ = stream.write_all(LOCK_MAGIC);
        }
    });
}

//=-- Called once the bind retries in `claim_single_instance` are exhausted:
//=-- connects to `port` and checks for `LOCK_MAGIC` to tell a genuine
//=-- kuma-remote instance apart from an unrelated occupant of the port.
fn classify_occupant(port: u16) -> SingleInstance {
    let addr = std::net::SocketAddr::from(([127, 0, 0, 1], port));
    let mut stream = match TcpStream::connect_timeout(&addr, HANDSHAKE_TIMEOUT) {
        Ok(stream) => stream,
        Err(_) => return SingleInstance::Unavailable,
    };
    if stream.set_read_timeout(Some(HANDSHAKE_TIMEOUT)).is_err() {
        return SingleInstance::Unavailable;
    }
    let mut buf = [0u8; LOCK_MAGIC.len()];
    match stream.read_exact(&mut buf) {
        Ok(()) if buf == *LOCK_MAGIC => SingleInstance::AlreadyRunning,
        _ => {
            warn!(
                port,
                "instance_lock_port is held by a process that isn't kuma-remote; \
                 proceeding without the single-instance guarantee"
            );
            SingleInstance::Unavailable
        }
    }
}

/// Subset of GitHub's release API response we care about.
#[derive(Debug, Deserialize)]
struct Release {
    assets: Vec<Asset>,
}

/// Subset of GitHub's release-asset API response we care about. `digest` is
/// GitHub-computed (`sha256:<hex>`) and present on any asset uploaded since
/// GitHub added artifact digests; it lets us compare hashes without
/// downloading the asset first.
#[derive(Debug, Deserialize)]
struct Asset {
    name: String,
    digest: Option<String>,
    browser_download_url: String,
}

/// Checks for a newer release and self-updates if `client` can reach GitHub
/// and the running exe's file name matches a release asset with a different
/// digest. Never fails startup: any error along the way is logged as a
/// warning and swallowed. `instance_lock` is this process's single-instance
/// claim (see [`SingleInstance`]), if any -- self-spawning a replacement is
/// only attempted while this is held; otherwise (including under
/// `service_mode`) an applied update just exits and relies on a supervisor
/// to restart it. On a successful update this calls [`std::process::exit`]
/// -- it does not return in that case.
pub async fn check_and_update(
    client: &Client,
    service_mode: bool,
    instance_lock: &mut Option<TcpListener>,
) {
    if let Err(err) = try_update(client, service_mode, instance_lock).await {
        warn!(error = %err, "Auto-update check failed, continuing with current version");
    }
}

/// Does the actual check-download-verify-replace-restart work. See the
/// module doc for the overall flow and its fail-open contract.
async fn try_update(
    client: &Client,
    service_mode: bool,
    instance_lock: &mut Option<TcpListener>,
) -> Result<()> {
    let (repo_owner, repo_name) = repository_owner_and_name()?;

    let exe_path = std::env::current_exe().context("Locating current executable")?;
    let exe_name = exe_path
        .file_name()
        .and_then(|name| name.to_str())
        .context("Current executable path has no file name")?;

    let release: Release = client
        .get(format!(
            "https://api.github.com/repos/{repo_owner}/{repo_name}/releases/latest"
        ))
        .header("Accept", "application/vnd.github+json")
        .send()
        .await
        .context("Requesting latest GitHub release")?
        .error_for_status()
        .context("GitHub release API returned an error status")?
        .json()
        .await
        .context("Parsing GitHub release response")?;

    let Some(asset) = release.assets.iter().find(|asset| asset.name == exe_name) else {
        info!(
            exe_name,
            "No matching release asset for this executable, skipping update check"
        );
        return Ok(());
    };

    let Some(remote_hash) = asset
        .digest
        .as_deref()
        .and_then(|digest| digest.strip_prefix("sha256:"))
    else {
        warn!(
            asset = %asset.name,
            "Latest release asset has no sha256 digest, skipping update check"
        );
        return Ok(());
    };

    let local_bytes = tokio::fs::read(&exe_path)
        .await
        .with_context(|| format!("Reading current executable {}", exe_path.display()))?;
    let local_hash = to_hex(&Sha256::digest(&local_bytes));

    if local_hash.eq_ignore_ascii_case(remote_hash) {
        info!("kuma-remote is up to date");
        return Ok(());
    }

    info!(
        local_hash,
        remote_hash, "Newer kuma-remote release found, downloading"
    );

    let new_bytes = client
        .get(&asset.browser_download_url)
        .send()
        .await
        .context("Downloading updated executable")?
        .error_for_status()
        .context("Download of updated executable returned an error status")?
        .bytes()
        .await
        .context("Reading downloaded executable body")?;

    let downloaded_hash = to_hex(&Sha256::digest(&new_bytes));
    if !downloaded_hash.eq_ignore_ascii_case(remote_hash) {
        anyhow::bail!(
            "Downloaded executable hash {downloaded_hash} does not match published digest {remote_hash}"
        );
    }

    //=-- Written next to the running exe (not a system temp dir) so the
    //=-- subsequent rename-based replace stays on the same filesystem/volume.
    let tmp_path = exe_path.with_extension("exe.new");
    tokio::fs::write(&tmp_path, &new_bytes)
        .await
        .with_context(|| format!("Writing downloaded executable to {}", tmp_path.display()))?;

    self_replace::self_replace(&tmp_path).context("Replacing running executable")?;
    //=-- Best-effort: self_replace has already copied the bytes into place,
    //=-- so a leftover temp file here is harmless clutter, not a correctness
    //=-- issue.
    let _ = tokio::fs::remove_file(&tmp_path).await;

    //=-- Self-spawning is only safe while we actually hold the single-instance
    //=-- lock: that's what guarantees a losing duplicate exits instead of
    //=-- piling up. Without it -- service_mode, or a claim that came back
    //=-- Unavailable/disabled -- fall back to exit-only and trust a
    //=-- supervisor (if any) to restart into the already-updated binary.
    if service_mode || instance_lock.is_none() {
        if !service_mode {
            warn!(
                "Update applied on disk, but no single-instance lock is held (instance_lock \
                 disabled or unavailable) -- skipping self-spawn to avoid risking an \
                 unprotected duplicate instance. A process supervisor, if any, must restart \
                 this process to pick up the update."
            );
        }
        info!("Update applied on disk; exiting so the process supervisor restarts into it");
        std::process::exit(0);
    }

    info!("Update applied, spawning replacement and exiting");
    let args: Vec<_> = std::env::args_os().skip(1).collect();
    let mut spawn_result = std::process::Command::new(&exe_path).args(&args).spawn();
    for _ in 1..SPAWN_ATTEMPTS {
        if spawn_result.is_ok() {
            break;
        }
        tokio::time::sleep(SPAWN_RETRY_DELAY).await;
        spawn_result = std::process::Command::new(&exe_path).args(&args).spawn();
    }
    if let Err(err) = spawn_result {
        error!(
            error = %err,
            "Update was applied to disk, but the replacement process failed to start after \
             {SPAWN_ATTEMPTS} attempts -- this process will keep running its OLD in-memory \
             code until it is manually restarted"
        );
        return Err(err).context("Spawning updated executable");
    }

    //=-- Only release the port once the replacement has actually been
    //=-- spawned: if every spawn attempt above had failed, we would have
    //=-- returned early and this process keeps running on its current
    //=-- in-memory code, in which case it must keep holding the lock it
    //=-- already holds rather than leaving itself unprotected for the rest
    //=-- of its run. The replacement's own claim attempt tolerates the brief
    //=-- remaining delay before this process fully exits (see
    //=-- `CLAIM_ATTEMPTS`/`CLAIM_RETRY_DELAY`).
    instance_lock.take();

    std::process::exit(0);
}

/// Lowercase-hex-encodes `bytes`, matching the format of GitHub's `digest`
/// field so the two can be compared directly.
fn to_hex(bytes: &[u8]) -> String {
    bytes.iter().map(|byte| format!("{byte:02x}")).collect()
}

/// Splits Cargo.toml's `package.repository` (e.g.
/// `https://github.com/imthatguyhere/kuma-remote`) into `(owner, name)`, so
/// the GitHub repo this binary checks for updates against has one source of
/// truth instead of being duplicated as separate constants here.
fn repository_owner_and_name() -> Result<(&'static str, &'static str)> {
    const REPOSITORY_URL: &str = env!("CARGO_PKG_REPOSITORY");
    let mut segments = REPOSITORY_URL.trim_end_matches('/').rsplit('/');
    let name = segments
        .next()
        .context("Cargo.toml `repository` is missing a repo name segment")?;
    let owner = segments
        .next()
        .context("Cargo.toml `repository` is missing an owner segment")?;
    Ok((owner, name))
}
