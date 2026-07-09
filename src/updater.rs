//! Startup self-updater: checks GitHub's latest release of this repo for a
//! release asset matching the currently running executable's file name, and
//! compares its GitHub-computed SHA-256 digest against the running exe's own
//! hash. On a mismatch, downloads the asset, verifies its hash, replaces the
//! running exe in place, and exits so the process supervisor -- a Windows
//! service manager, a systemd unit with `Restart=`, etc. -- restarts it into
//! the new binary.
//!
//! Deliberately does not spawn a replacement process itself: doing so races
//! a supervisor's own restart-on-exit behavior. A supervisor that restarts
//! on any exit (e.g. NSSM's default `AppExit`, or a systemd unit with
//! `Restart=always`) would then run both our self-spawned child and its own
//! fresh instance, permanently doubling up on every update. Exiting and
//! leaving the restart entirely to whatever supervises the process keeps
//! exactly one instance running in every case, at the cost of the update
//! only taking effect once something actually restarts kuma-remote.
//!
//! Every failure mode here (network, rate limiting, missing digest, no
//! matching asset, permissions, ...) is logged and swallowed rather than
//! propagated -- a failed or skipped update check must never prevent
//! kuma-remote from starting its configured checks.

use anyhow::{Context, Result};
use reqwest::Client;
use serde::Deserialize;
use sha2::{Digest, Sha256};
use tracing::{info, warn};

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
/// warning and swallowed. On a successful update this calls
/// [`std::process::exit`] after replacing the binary on disk, relying on the
/// process supervisor to restart into it -- it does not return, and does not
/// spawn a replacement itself (see module docs for why).
pub async fn check_and_update(client: &Client) {
    if let Err(err) = try_update(client).await {
        warn!(error = %err, "Auto-update check failed, continuing with current version");
    }
}

/// Does the actual check-download-verify-replace-restart work. See the
/// module doc for the overall flow and its fail-open contract.
async fn try_update(client: &Client) -> Result<()> {
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

    let local_bytes = std::fs::read(&exe_path)
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

    // Written next to the running exe (not a system temp dir) so the
    // subsequent rename-based replace stays on the same filesystem/volume.
    let tmp_path = exe_path.with_extension("exe.new");
    std::fs::write(&tmp_path, &new_bytes)
        .with_context(|| format!("Writing downloaded executable to {}", tmp_path.display()))?;

    self_replace::self_replace(&tmp_path).context("Replacing running executable")?;
    // Best-effort: self_replace has already copied the bytes into place, so
    // a leftover temp file here is harmless clutter, not a correctness issue.
    let _ = std::fs::remove_file(&tmp_path);

    info!("Update applied on disk; exiting so the process supervisor restarts into it");
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
