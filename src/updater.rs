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
//! supervising it at all — which must keep working, since that's a normal
//! way to run this tool. The risk that creates — a supervisor that *also*
//! restarts on exit (NSSM's default `AppExit` behavior, or a systemd unit
//! with `Restart=always`) ending up running both the self-spawned
//! replacement and its own fresh instance, permanently, after every update
//! — is closed by [`SingleInstance`]: every process, whether spawned by us
//! or by a supervisor, must claim a fixed loopback port before doing any
//! real work. Only one process can ever hold it; whichever one loses just
//! exits immediately. Since the exe on disk is already the new version by
//! the time anyone is racing for the port, it doesn't matter which one wins
//! — exactly one instance of the new version ends up running either way.
//!
//! Self-spawning a replacement is only safe when this process actually holds
//! the single-instance lock (a live [`SingleInstance::Claimed`]): that's what
//! guarantees a losing duplicate exits instead of piling up. If the lock
//! isn't held — `instance_lock: false`, or a claim that came back
//! [`SingleInstance::Unavailable`] — self-spawning would be unprotected, so
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
//! The lock port also doubles as a tiny graceful-shutdown channel, since a
//! self-spawned replacement is a detached process a terminal's Ctrl-C can no
//! longer reliably reach once the original foreground process has exited
//! (it's no longer the shell's tracked job, even though it's still attached
//! to the same console). `kuma-remote --stop` ([`request_stop`]) connects to
//! the lock port and asks whichever instance holds it to shut down; the
//! handshake responder spawned by [`claim_single_instance`] recognizes the
//! request and notifies `main`'s shutdown wait, the same one Ctrl-C also
//! feeds into. Only meaningful when the lock is actually in use -- under
//! `service_mode`, or with `instance_lock: false`, there's no listener to
//! connect to, and Ctrl-C already works reliably anyway since nothing is
//! ever self-spawned in either of those cases.
//!
//! Every failure mode here (network, rate limiting, missing digest, no
//! matching asset, permissions, ...) is logged and swallowed rather than
//! propagated — a failed or skipped update check must never prevent
//! kuma-remote from starting its configured checks.

use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use reqwest::{Client, Response};
use serde::Deserialize;
use sha2::{Digest, Sha256};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::sync::Notify;
use tracing::{error, info, warn};

//=-- Retry budget for a port bind that fails with `AddrInUse`, before
//=-- concluding another instance is genuinely running. Covers the brief
//=-- window between an updater spawning its replacement and that
//=-- replacement claiming the port for itself.
const CLAIM_ATTEMPTS: u32 = 10;
const CLAIM_RETRY_DELAY: Duration = Duration::from_millis(100);

//=-- How long to wait for the identity handshake (see `LOCK_MAGIC`) when
//=-- another process already holds the lock port. Generous relative to the
//=-- handshake responder's own work (writing a few dozen bytes) so a
//=-- momentarily busy host (e.g. antivirus scanning the two exes involved in
//=-- a self-update) doesn't misclassify a genuine occupant as unrelated.
const HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(5);

//=-- Bytes the lock holder writes back to any connection on the lock port,
//=-- so a challenger that loses the bind race can tell "another kuma-remote
//=-- instance holds this" apart from "some unrelated process/service
//=-- happens to be bound to this port".
const LOCK_MAGIC: &[u8] = b"kuma-remote-single-instance-v1";

//=-- Exact bytes `request_stop` sends, and the handshake responder requires
//=-- an exact match on, after the identity handshake (`LOCK_MAGIC`) and
//=-- before treating an inbound connection as a genuine shutdown request
//=-- rather than an incidental local connection -- e.g. `classify_occupant`'s
//=-- own probe, which reads `LOCK_MAGIC` and disconnects without writing
//=-- anything back.
const STOP_COMMAND: &[u8] = b"kuma-remote-stop-v1\n";
//=-- Bytes the handshake responder writes back once `STOP_COMMAND` is
//=-- matched, confirming to `request_stop` that a shutdown was actually
//=-- triggered rather than the connection just being dropped.
const STOP_ACK: &[u8] = b"kuma-remote-stopping-v1\n";
//=-- How long the handshake responder waits, after writing `LOCK_MAGIC`, for
//=-- the connecting side to send `STOP_COMMAND` before giving up and closing
//=-- the connection. Also the budget `request_stop` allows for the
//=-- acknowledgement that follows.
const STOP_COMMAND_TIMEOUT: Duration = Duration::from_secs(2);

//=-- How many times, and how far apart, `try_update` retries spawning the
//=-- replacement process before giving up and logging it as stuck. Also
//=-- doubles as the grace period given to a freshly spawned replacement
//=-- before checking it hasn't already exited (see `try_update`).
const SPAWN_ATTEMPTS: u32 = 3;
const SPAWN_RETRY_DELAY: Duration = Duration::from_millis(250);

//=-- Ceiling on the release-asset download itself, decoupled from the
//=-- shared client's blanket per-request timeout (see `main.rs`) so a slow
//=-- but still-progressing download isn't killed early; progress is instead
//=-- logged periodically (see `DOWNLOAD_PROGRESS_INTERVAL`).
const DOWNLOAD_TIMEOUT: Duration = Duration::from_secs(30 * 60);

//=-- How often to log download progress while streaming the release asset.
const DOWNLOAD_PROGRESS_INTERVAL: Duration = Duration::from_secs(5);

//=-- Safety cap on the release asset size, checked against the response's
//=-- `Content-Length` up front and against actual bytes received as they
//=-- arrive (in case that header is absent or wrong). Generous headroom over
//=-- a release binary of a few MB — this only guards against a
//=-- misconfigured or unexpectedly huge asset causing an unbounded
//=-- in-memory allocation, not against a legitimately larger future build.
const MAX_DOWNLOAD_SIZE: u64 = 200 * 1024 * 1024;

/// Outcome of [`claim_single_instance`].
pub enum SingleInstance {
    /// This process holds the lock. Keep `listener` alive for as long as
    /// this process should count as "the" running instance; dropping it
    /// (including via [`Option::take`]) releases the claim immediately.
    /// `stop_requested` is notified from the handshake responder thread the
    /// moment a `--stop` request is validated (see [`request_stop`]); the
    /// caller should race it against `tokio::signal::ctrl_c()` to begin a
    /// graceful shutdown either way.
    Claimed {
        listener: TcpListener,
        stop_requested: Arc<Notify>,
    },
    /// Another instance already holds the lock; this process must not do
    /// any real work.
    AlreadyRunning,
    /// The lock could not be claimed for a reason unrelated to another
    /// instance running (e.g. a local permissions/network-stack issue, or
    /// the port being held by an unrelated process). Treated as "proceed
    /// anyway, without the guarantee" — this safety net must never itself
    /// block kuma-remote from starting.
    Unavailable,
}

/// Tries to claim `port` (see `Config::instance_lock_port`) as a
/// cross-process single-instance mutex. Retries briefly on `AddrInUse` (see
/// [`CLAIM_ATTEMPTS`]) before concluding another instance is running. Runs
/// blocking I/O throughout — callers on a tokio runtime should invoke this
/// via `tokio::task::spawn_blocking` rather than calling it directly from an
/// async context.
pub fn claim_single_instance(port: u16) -> SingleInstance {
    for attempt in 0..CLAIM_ATTEMPTS {
        match TcpListener::bind(("127.0.0.1", port)) {
            Ok(listener) => {
                let stop_requested = Arc::new(Notify::new());
                spawn_handshake_responder(&listener, Arc::clone(&stop_requested));
                return SingleInstance::Claimed {
                    listener,
                    stop_requested,
                };
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

//=-- Spawns a detached background thread that, for every connection on
//=-- `listener`: (1) writes `LOCK_MAGIC`, so a challenger (`classify_occupant`
//=-- or `request_stop`) can confirm this is genuinely a kuma-remote instance
//=-- holding the port, then (2) waits up to `STOP_COMMAND_TIMEOUT` for the
//=-- connecting side to send `STOP_COMMAND` in reply -- if it does, writes
//=-- `STOP_ACK` and calls `stop_requested.notify_one()`. A bare identity
//=-- probe that never writes anything back (like `classify_occupant`'s) just
//=-- times out and moves on to the next connection, same as any other
//=-- unrelated/incidental connection to this loopback port. Runs for as long
//=-- as `listener` (or its clone) stays open; best-effort, so a failure to
//=-- clone just means a challenger later treats this occupant as
//=-- unidentifiable rather than confirmed, and `--stop` has nothing to
//=-- connect to.
fn spawn_handshake_responder(listener: &TcpListener, stop_requested: Arc<Notify>) {
    let responder = match listener.try_clone() {
        Ok(responder) => responder,
        Err(err) => {
            warn!(error = %err, "Could not start single-instance handshake responder");
            return;
        }
    };
    std::thread::spawn(move || {
        for mut stream in responder.incoming().flatten() {
            if stream.write_all(LOCK_MAGIC).is_err() {
                continue;
            }
            if stream.set_read_timeout(Some(STOP_COMMAND_TIMEOUT)).is_err() {
                continue;
            }
            let mut buf = vec![0u8; STOP_COMMAND.len()];
            if stream.read_exact(&mut buf).is_ok() && buf == STOP_COMMAND {
                let _ = stream.write_all(STOP_ACK);
                stop_requested.notify_one();
            }
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

/// Connects to `port` (see `Config::instance_lock_port`) and asks whichever
/// process holds it to shut down gracefully. Checks the occupant's identity
/// the same way `classify_occupant` does (via [`LOCK_MAGIC`]) before ever
/// sending [`STOP_COMMAND`], so this can't be pointed at an unrelated
/// process that happens to be listening on that port. Returns `Ok(true)` if
/// a kuma-remote instance acknowledged the request, `Ok(false)` if nothing
/// is listening on `port` at all (no running instance to stop), or `Err`
/// for any other connection/protocol failure -- including the occupant not
/// being a kuma-remote instance, or one that didn't acknowledge in time
/// (e.g. an older build without this feature, which would just drop the
/// connection after the identity handshake).
pub async fn request_stop(port: u16) -> Result<bool> {
    let mut stream = match tokio::net::TcpStream::connect(("127.0.0.1", port)).await {
        Ok(stream) => stream,
        Err(err) if err.kind() == std::io::ErrorKind::ConnectionRefused => return Ok(false),
        Err(err) => return Err(err).context("Connecting to running kuma-remote instance"),
    };

    let mut magic = vec![0u8; LOCK_MAGIC.len()];
    tokio::time::timeout(HANDSHAKE_TIMEOUT, stream.read_exact(&mut magic))
        .await
        .context("Timed out waiting for instance identity handshake")?
        .context("Reading instance identity handshake")?;
    if magic != LOCK_MAGIC {
        anyhow::bail!("Port {port} is held by a process that isn't kuma-remote");
    }

    stream
        .write_all(STOP_COMMAND)
        .await
        .context("Sending stop request")?;

    let mut ack = vec![0u8; STOP_ACK.len()];
    tokio::time::timeout(STOP_COMMAND_TIMEOUT, stream.read_exact(&mut ack))
        .await
        .context("Timed out waiting for stop acknowledgement")?
        .context("Reading stop acknowledgement")?;

    Ok(ack == STOP_ACK)
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

/// Whether the caller — ultimately `main` — should continue starting
/// checks normally, or exit immediately because an update was applied.
/// Returned instead of calling [`std::process::exit`] directly, so the
/// actual exit happens in `main`'s own control flow rather than being
/// bypassed there: any cleanup `main` gains in the future runs on this path
/// the same as any other, since it's a normal return, not a hard exit.
pub enum UpdateOutcome {
    /// No update was applied (already up to date, none available, or the
    /// check failed); proceed with startup as normal.
    Continue,
    /// An update was applied on disk. Whether or not a replacement process
    /// was spawned, this process's job is done — the caller should stop
    /// startup and return, letting the current process exit normally.
    Exit,
}

/// Checks for a newer release and self-updates if `client` can reach GitHub
/// and the running exe's file name matches a release asset with a different
/// digest. Never fails startup: any error along the way is logged as a
/// warning and swallowed, returning [`UpdateOutcome::Continue`].
/// `instance_lock` is this process's single-instance claim (see
/// [`SingleInstance`]), if any — self-spawning a replacement is only
/// attempted while this is held; otherwise (including under `service_mode`)
/// an applied update just returns [`UpdateOutcome::Exit`] and relies on a
/// supervisor to restart it.
pub async fn check_and_update(
    client: &Client,
    service_mode: bool,
    instance_lock: &mut Option<TcpListener>,
) -> UpdateOutcome {
    match try_update(client, service_mode, instance_lock).await {
        Ok(outcome) => outcome,
        Err(err) => {
            let is_timeout = err
                .chain()
                .filter_map(|cause| cause.downcast_ref::<reqwest::Error>())
                .any(reqwest::Error::is_timeout);
            if is_timeout {
                warn!("Update check failed: Timeout");
            } else {
                warn!(error = %err, "Auto-update check failed, continuing with current version");
            }
            UpdateOutcome::Continue
        }
    }
}

/// Does the actual check-download-verify-replace-restart work. See the
/// module doc for the overall flow and its fail-open contract.
async fn try_update(
    client: &Client,
    service_mode: bool,
    instance_lock: &mut Option<TcpListener>,
) -> Result<UpdateOutcome> {
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
        return Ok(UpdateOutcome::Continue);
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
        return Ok(UpdateOutcome::Continue);
    };

    let local_bytes = tokio::fs::read(&exe_path)
        .await
        .with_context(|| format!("Reading current executable {}", exe_path.display()))?;
    //=-- Hashing a multi-MB executable is CPU-bound; run it on a blocking
    //=-- thread rather than stalling the async runtime worker.
    let local_hash = tokio::task::spawn_blocking(move || to_hex(&Sha256::digest(&local_bytes)))
        .await
        .context("Hashing current executable panicked")?;

    if local_hash.eq_ignore_ascii_case(remote_hash) {
        info!("kuma-remote is up to date");
        return Ok(UpdateOutcome::Continue);
    }

    info!(
        local_hash,
        remote_hash, "Newer kuma-remote release found, downloading"
    );

    let response = client
        .get(&asset.browser_download_url)
        .timeout(DOWNLOAD_TIMEOUT)
        .send()
        .await
        .context("Downloading updated executable")?
        .error_for_status()
        .context("Download of updated executable returned an error status")?;

    if let Some(total) = response.content_length()
        && total > MAX_DOWNLOAD_SIZE
    {
        anyhow::bail!(
            "Release asset {} is {total} bytes, exceeding the {MAX_DOWNLOAD_SIZE}-byte safety cap",
            asset.name
        );
    }

    let (new_bytes, downloaded_hash) = download_with_progress(response).await?;
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

    if let Err(err) = self_replace::self_replace(&tmp_path) {
        //=-- Best-effort: nothing was applied, so the temp file is just
        //=-- disk clutter — clean it up before propagating the real error.
        let _ = tokio::fs::remove_file(&tmp_path).await;
        return Err(err).context("Replacing running executable");
    }
    //=-- Best-effort: self_replace has already copied the bytes into place,
    //=-- so a leftover temp file here is harmless clutter, not a correctness
    //=-- issue.
    let _ = tokio::fs::remove_file(&tmp_path).await;

    //=-- Self-spawning is only safe while we actually hold the single-instance
    //=-- lock: that's what guarantees a losing duplicate exits instead of
    //=-- piling up. Without it — service_mode, or a claim that came back
    //=-- Unavailable/disabled — fall back to exit-only and trust a
    //=-- supervisor (if any) to restart into the already-updated binary.
    if service_mode || instance_lock.is_none() {
        if !service_mode {
            warn!(
                "Update applied on disk, but no single-instance lock is held (instance_lock \
                 disabled or unavailable) — skipping self-spawn to avoid risking an \
                 unprotected duplicate instance. A process supervisor, if any, must restart \
                 this process to pick up the update."
            );
        }
        info!("Update applied on disk; exiting so the process supervisor restarts into it");
        return Ok(UpdateOutcome::Exit);
    }

    info!("Update applied, spawning replacement and exiting");
    let args: Vec<_> = std::env::args_os().skip(1).collect();

    let mut child = None;
    for attempt in 1..=SPAWN_ATTEMPTS {
        let mut candidate = match std::process::Command::new(&exe_path).args(&args).spawn() {
            Ok(candidate) => candidate,
            Err(err) => {
                warn!(attempt, error = %err, "Failed to spawn replacement process, retrying");
                if attempt < SPAWN_ATTEMPTS {
                    tokio::time::sleep(SPAWN_RETRY_DELAY).await;
                }
                continue;
            }
        };
        //=-- spawn() only confirms the OS accepted the launch, not that the
        //=-- replacement is actually running — a crash right after start
        //=-- (e.g. antivirus briefly quarantining the just-written exe) would
        //=-- otherwise look identical to success, and this process is about
        //=-- to release the lock and exit on that belief. Give it a moment,
        //=-- then confirm it hasn't already exited before trusting it.
        tokio::time::sleep(SPAWN_RETRY_DELAY).await;
        match candidate.try_wait() {
            Ok(None) => {
                child = Some(candidate);
                break;
            }
            Ok(Some(status)) => {
                warn!(
                    attempt,
                    %status,
                    "Replacement process exited immediately after spawning, retrying"
                );
            }
            Err(err) => {
                warn!(
                    attempt,
                    error = %err,
                    "Could not confirm replacement process is still running, assuming it is"
                );
                child = Some(candidate);
                break;
            }
        }
    }
    let Some(_child) = child else {
        error!(
            "Update was applied to disk, but the replacement process failed to stay running \
             after {SPAWN_ATTEMPTS} attempts — this process will keep running its OLD in-memory \
             code until it is manually restarted"
        );
        anyhow::bail!("Replacement process did not stay running after {SPAWN_ATTEMPTS} attempts");
    };

    //=-- Only release the port once the replacement has actually been
    //=-- spawned: if every spawn attempt above had failed, we would have
    //=-- returned early and this process keeps running on its current
    //=-- in-memory code, in which case it must keep holding the lock it
    //=-- already holds rather than leaving itself unprotected for the rest
    //=-- of its run. The replacement's own claim attempt tolerates the brief
    //=-- remaining delay before this process fully exits (see
    //=-- `CLAIM_ATTEMPTS`/`CLAIM_RETRY_DELAY`).
    instance_lock.take();

    Ok(UpdateOutcome::Exit)
}

/// Streams `response`'s body to completion, hashing it incrementally (so no
/// separate full-buffer digest pass is needed afterward) and logging
/// progress every [`DOWNLOAD_PROGRESS_INTERVAL`] instead of blocking
/// silently for however long the download takes. Enforces
/// [`MAX_DOWNLOAD_SIZE`] against bytes actually received, in case
/// `Content-Length` was absent or wrong. Returns the body bytes and their
/// lowercase-hex SHA-256 digest.
async fn download_with_progress(mut response: Response) -> Result<(Vec<u8>, String)> {
    let total_len = response.content_length();
    let mut downloaded = Vec::new();
    let mut hasher = Sha256::new();
    let mut last_log = Instant::now();

    while let Some(chunk) = response
        .chunk()
        .await
        .context("Reading downloaded executable body")?
    {
        downloaded.extend_from_slice(&chunk);
        if downloaded.len() as u64 > MAX_DOWNLOAD_SIZE {
            anyhow::bail!(
                "Downloaded {} bytes, exceeding the {MAX_DOWNLOAD_SIZE}-byte safety cap",
                downloaded.len()
            );
        }
        hasher.update(&chunk);

        if last_log.elapsed() >= DOWNLOAD_PROGRESS_INTERVAL {
            match total_len {
                Some(total) => info!(downloaded = downloaded.len(), total, "Downloading update"),
                None => info!(downloaded = downloaded.len(), "Downloading update"),
            }
            last_log = Instant::now();
        }
    }

    let hash = to_hex(&hasher.finalize());
    Ok((downloaded, hash))
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
