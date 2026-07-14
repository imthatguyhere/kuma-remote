# Functionality

> Module list adapted from the CLAUDE.md template: this project has no `installed.rs`/`cdk_info.rs` (those belong to a different, software-inventory project). Sections below cover this project's actual modules: `main.rs`, `config.rs`, `scheduler.rs`, `checks/ping.rs`, `checks/heartbeat.rs`, `kuma.rs`, `logging.rs`, `updater.rs`.

## Data Flow

1. `main.rs` initializes logging, then immediately logs the app name, version, and author(s) (via `CARGO_PKG_NAME`/`CARGO_PKG_VERSION`/`CARGO_PKG_AUTHORS`), before any config is loaded.
2. `main.rs` parses CLI args, resolves the config path via `resolve_config_path` (the explicit `--config` value, or else the first of `kuma-remote.yaml`/`kuma-config.yaml`/`config.yaml` that exists), then calls `config::Config::load` with it.
3. `config::Config::load` reads the file, deserializes it as StrictYAML into `Config`, normalizes it, and validates it (non-empty, unique `id`s, non-zero `interval`s, `host` present for `ping` checks).
4. If `config.debug` is set, `main.rs` logs every configured check before starting.
5. Unless `config.service_mode` is true or `config.instance_lock` is false, `main.rs` calls `updater::claim_single_instance(config.instance_lock_port)`. If another instance already holds the lock, it logs a warning and returns immediately -- no checks start. Otherwise it holds onto the result (a claimed listener, or `None` if the claim was skipped or disabled) for the rest of the process.
6. `main.rs` builds a single shared `reqwest::Client`. If `config.auto_update` is set, it then calls `updater::check_and_update` with that client, `config.service_mode`, and the single-instance lock before doing anything else -- see `updater.rs` below. On an applied update this exits the process (in the default, non-`service_mode`, path: after spawning a replacement that inherits the single-instance claim); otherwise it logs and falls through to normal startup regardless of outcome.
7. `main.rs` calls `scheduler::spawn_all` (passing `config.debug` and `config.report_run_failures` through), which spawns one tokio task per `CheckConfig`.
8. Each task loops: wait for its `interval` tick, run the check for its `mode` (`ping` requires `host` and reports `Up`/`Down` based on the ping result; `heartbeat` always reports `Up` with message `"Heartbeat"`, optionally including a latency if `host` is set and the ping succeeds), translate the result into a `kuma::PushStatus`, and call `kuma::push`. If the run itself errors out (rather than completing with an `Up`/`Down` result), the error is logged and, when `report_run_failures` is set, also pushed to Kuma as a `down` status with the error as `msg`.
9. `kuma::push` builds the final push URL (`status`/`msg`/`ping` query parameters appended to `push_url`), logs it if `debug` is set, sends the GET request, and treats a non-2xx response as an error.
10. `main.rs` blocks on Ctrl-C; on receipt, it aborts all spawned check tasks and exits.

## Configuration

Single StrictYAML file. Path is set via `--config`, or else resolved by `main::resolve_config_path` to the first existing file among `kuma-remote.yaml`, `kuma-config.yaml`, `config.yaml` (falling back to `kuma-remote.yaml` if none exist, so the load error names a sensible file). Schema: a top-level `checks` sequence of mappings, each deserialized into `config::CheckConfig`. `interval` is parsed via `humantime_serde` from strings like `"30s"`/`"5m"`/`"1h"`. Deserialization uses `strict_yaml_rust::serde::from_str`. `auto_update` (`bool`, default `true` via `default_auto_update`) gates whether `updater::check_and_update` runs at all; `service_mode` (`bool`, default `false`) and `instance_lock`/`instance_lock_port` (`bool`/`u16`, defaults `true`/`51247` via `default_instance_lock`/`default_instance_lock_port`) control the single-instance guard and the updater's restart strategy -- see `updater.rs`. `Config::normalize` also warns if `instance_lock_port` is `0` while the lock is actually in effect (`instance_lock` true and `service_mode` false), since that would make every bind attempt land on a different OS-assigned port.

The only other configuration source is the `RUST_LOG` environment variable, read by `logging::init` via `tracing_subscriber::EnvFilter`.

## Build and release packaging

No non-default build behavior beyond `build.rs` (Windows icon resource via `winresource`) and the `[profile.release]` tuning in `Cargo.toml`. There is no CI/release pipeline; `scripts/build-release.ps1` builds and copies the exe into `dist/` for manual upload to a GitHub release. `updater.rs` relies on GitHub computing a SHA-256 `digest` for each uploaded release asset itself (returned by the releases API) -- no project-side checksum file is generated or required.

## `main.rs`

Purpose: process entry point -- CLI parsing, config load, task orchestration, shutdown.

Types:

- `Cli` -- `clap::Parser` struct; single field `config: Option<PathBuf>` (`-c`/`--config`, no default -- absence triggers the `DEFAULT_CONFIG_CANDIDATES` lookup).

Constants:

- `DEFAULT_CONFIG_CANDIDATES: [&str; 3]` -- `["kuma-remote.yaml", "kuma-config.yaml", "config.yaml"]`, tried in order when `--config` is not given.

Functions:

- `resolve_config_path(explicit: Option<PathBuf>) -> PathBuf` -- returns `explicit` if given; otherwise returns the first of `DEFAULT_CONFIG_CANDIDATES` that exists on disk, or the first candidate (`kuma-remote.yaml`) if none exist.
- `main() -> anyhow::Result<()>` (async, `#[tokio::main]`) -- initializes logging, logs the app name/version/authors (`env!("CARGO_PKG_NAME"/"CARGO_PKG_VERSION"/"CARGO_PKG_AUTHORS")`), parses CLI args, resolves the config path via `resolve_config_path`, loads config, logs every check when `config.debug` is set, then, unless `config.service_mode` is true or `config.instance_lock` is false, calls `updater::claim_single_instance(config.instance_lock_port)` and returns immediately (logging a warning) if another instance already holds the lock. Builds the shared `reqwest::Client` (with a desktop-Chrome `User-Agent` override -- see below), runs `updater::check_and_update` (passing `config.service_mode` and the single-instance lock through) when `config.auto_update` is set, spawns the scheduler with `config.debug` and `config.report_run_failures`, waits on `tokio::signal::ctrl_c()`, then aborts all check task handles before returning.

Key detail: the `reqwest::Client` is built with `.user_agent(...)` set to a real desktop Chrome-on-Windows string instead of reqwest's default `reqwest/x.y.z`. Some reverse proxies / WAFs (e.g. Cloudflare bot protection) block generic HTTP-client user agents while allowing browsers, which otherwise manifests as push requests failing (404/403) even though the same URL works from a browser.

## `config.rs`

Purpose: StrictYAML config schema and validation.

Types:

- `Config { debug: bool, report_run_failures: bool, auto_update: bool, service_mode: bool, instance_lock: bool, instance_lock_port: u16, checks: Vec<CheckConfig> }` -- `debug` and `service_mode` use `#[serde(default)]` (default `false`); `report_run_failures` uses `#[serde(default = "default_report_run_failures")]` (defaults to `true`); `auto_update` uses `#[serde(default = "default_auto_update")]` (defaults to `true`); `instance_lock` uses `#[serde(default = "default_instance_lock")]` (defaults to `true`); `instance_lock_port` uses `#[serde(default = "default_instance_lock_port")]` (defaults to `51247`).
- `CheckConfig { id: String, name: String, mode: CheckMode, host: Option<String>, push_url: String, interval: Duration }` -- `interval` uses `#[serde(with = "humantime_serde")]`; `host` uses `#[serde(default)]` since it's only required for `CheckMode::Ping`.
- `CheckMode` -- enum with variants `Ping` and `Heartbeat` (`#[serde(rename_all = "lowercase")]`, so the YAML scalars `ping`/`heartbeat` map to them). New check modes extend this enum.

Public functions:

- `Config::load(path: &Path) -> anyhow::Result<Config>` -- reads the file, deserializes via `strict_yaml_rust::serde::from_str`, calls `normalize`, then `validate`.

Key internal functions:

- `Config::normalize(&mut self)` -- corrects known copy-paste mistakes that have one unambiguous fix: if a `push_url` contains `?` (Kuma's dashboard shows the push URL with a `?status=up&msg=OK&ping=` example suffix attached, which users sometimes copy verbatim), it's truncated at the `?` and a warning is logged with the check id and original value. Also warns (without changing anything) if `instance_lock_port` is `0` while the single-instance lock is actually in effect (`instance_lock` true and `service_mode` false), since a `0` port lets the OS assign a different one on every bind, defeating the lock.
- `Config::validate(&self) -> anyhow::Result<()>` -- rejects an empty `checks` list, duplicate `id`s (via a `HashSet` of seen ids), any `interval` that is zero, and any `CheckMode::Ping` check whose `host` is `None`.
- `default_report_run_failures() -> bool` -- serde default-value function for `Config::report_run_failures`; returns `true`.
- `default_auto_update() -> bool` -- serde default-value function for `Config::auto_update`; returns `true`.
- `default_instance_lock() -> bool` -- serde default-value function for `Config::instance_lock`; returns `true`.
- `default_instance_lock_port() -> u16` -- serde default-value function for `Config::instance_lock_port`; returns `51247`.

## `scheduler.rs`

Purpose: runs each check on its own independent interval and drives the check -> push pipeline.

Public functions:

- `spawn_all(checks: Vec<CheckConfig>, client: Client, debug: bool, report_run_failures: bool) -> Vec<JoinHandle<()>>` -- spawns one `tokio::spawn`ed task per check (each with its own cloned `Client`) and returns their handles. `debug` and `report_run_failures` are threaded through to every check loop.

Key internal functions:

- `run_check_loop(check: CheckConfig, client: Client, debug: bool, report_run_failures: bool)` -- owns a `tokio::time::interval(check.interval)` (with `MissedTickBehavior::Delay`, so a slow check run doesn't cause a burst of catch-up ticks) and calls `run_once` on every tick, logging (not propagating) any error so one bad run doesn't kill the loop. When `report_run_failures` is set, a run error is also pushed to Kuma as `PushStatus::Down { message: "Check run failed: {err}" }`; a failure to push that is itself logged (and swallowed).
- `run_once(check: &CheckConfig, client: &Client, debug: bool) -> anyhow::Result<()>` -- matches on `check.mode`: `CheckMode::Ping` unwraps `check.host` (present per `Config::validate`), runs `ping::ping_once`, and maps its `Up`/`Down` outcome straight to `PushStatus::Up { ping_ms: Some(_), message: None }` / `PushStatus::Down`; `CheckMode::Heartbeat` calls `heartbeat::beat_once(check.host.clone())` and always builds `PushStatus::Up { ping_ms: <from the outcome>, message: Some("Heartbeat") }`. Either way, the outcome is logged and the resulting `kuma::PushStatus` is pushed.

Algorithm: per-check tasks are fully independent (`tokio::spawn` per check, not a shared single loop), so no check's schedule or latency affects any other check's timing.

## `checks/ping.rs`

Purpose: the `ping` check mode -- a single ICMP echo per run.

Types:

- `PingOutcome` -- `Up { latency_ms: f64 }` or `Down { reason: String }`.

Constants:

- `RECV_TIMEOUT: Duration = 5s` -- upper bound on how long `ping_once` waits for a result from the `pinger` channel, independent of any OS-level ping timeout.

Public functions:

- `ping_once(host: String) -> anyhow::Result<PingOutcome>` (async) -- runs the blocking ping logic on a `tokio::task::spawn_blocking` thread and awaits it.

Key internal functions:

- `ping_once_blocking(host: &str) -> anyhow::Result<PingOutcome>` -- builds `pinger::PingOptions`, starts `pinger::ping`, and classifies the first `PingResult` received (`Pong` -> `Up`, `Timeout`/`Unknown`/`PingExited` -> `Down` with a reason), or `Down` on a `recv_timeout` expiry.

Key algorithm / platform behavior: `pinger` shells out to the OS `ping` binary on Unix (or uses the native `IcmpSendEcho` API via `winping` on Windows, in-process, no subprocess). On Unix, plain `ping` streams echoes forever, so taking one result and dropping the channel would otherwise leave the child `ping` process and its reader thread running forever. To prevent that leak, Linux builds add `-c 1 -W 2` and BSD-family builds (`macos`/`freebsd`/`openbsd`/`netbsd`/`dragonfly`) add `-c 1 -t 2` (BSD ping's `-t` is a wait-timeout, not TTL) via `PingOptions::with_raw_arguments`, so the process always exits after one probe. Windows needs no such flag: `winping`'s `IcmpSendEcho` call has its own built-in 2s timeout and never spawns a subprocess.

## `checks/heartbeat.rs`

Purpose: the `heartbeat` check mode -- always reports the process is alive, with an optional ping-derived latency.

Types:

- `HeartbeatOutcome { latency_ms: Option<f64> }` -- always maps to `Up` in `scheduler::run_once`; `latency_ms` is `None` when no `host` was configured, or when the ping to it failed or timed out.

Public functions:

- `beat_once(host: Option<String>) -> HeartbeatOutcome` (async) -- when `host` is `Some`, calls `ping::ping_once` and keeps the latency only on `PingOutcome::Up`; `PingOutcome::Down` and a propagated `ping_once` error are both treated as "no latency available", not as a failed heartbeat, and are logged via `tracing::warn!` (with the host and reason/error) so a misconfigured or unreachable diagnostic host is still visible in logs. When `host` is `None`, returns `latency_ms: None` directly without pinging.

Key algorithm: unlike `checks/ping.rs`, this check's `host` is diagnostic, not load-bearing -- its purpose is proving the `kuma-remote` process is running, so a failed ping only drops the latency figure and never turns the heartbeat down.

## `kuma.rs`

Purpose: Uptime Kuma push-monitor HTTP client.

Types:

- `PushStatus` -- `Up { ping_ms: Option<f64>, message: Option<String> }` (message defaults to `"OK"` when `None`, sent as `"Heartbeat"` by `CheckMode::Heartbeat`) or `Down { message: String }`.

Constants:

- `MAX_MESSAGE_LEN: usize = 250` -- Kuma's approximate cap on the `msg` query parameter; longer messages are truncated before sending.

Public functions:

- `push(client: &Client, push_url: &str, status: PushStatus, debug: bool) -> anyhow::Result<()>` (async) -- parses `push_url` into a `reqwest::Url`, appends `status`/`msg` (`msg` defaults to `"OK"` for `PushStatus::Up` when its `message` is `None`, and is truncated via `truncate` for both `Up` and `Down`) and `ping` when known as query pairs, logs the final URL when `debug` is set, sends the GET request, and errors on a non-2xx response.

Key internal functions:

- `truncate(s: &str, max_len: usize) -> String` -- truncates on a `char_indices` boundary (never splits a multi-byte UTF-8 character) to at most `max_len` characters.

Note: the URL is built explicitly via `Url::query_pairs_mut()` rather than `RequestBuilder::query()` so the exact final URL string is available to log before the request is sent (`RequestBuilder` doesn't expose its built URL without consuming itself).

## `logging.rs`

Purpose: process-wide `tracing` subscriber setup.

Public functions:

- `init()` -- installs a `tracing_subscriber::fmt` subscriber using `EnvFilter` from `RUST_LOG`, defaulting to `info` if unset or invalid.

## `updater.rs`

Purpose: startup self-updater -- checks GitHub's latest release for a newer build of the running executable, replaces it in place when found, and either spawns a replacement process running the new binary and exits (default), or just exits and trusts a process supervisor to restart it (`service_mode`). In the default mode, a single-instance lock (a configurable loopback TCP port) makes self-spawning safe even if a process supervisor *also* restarts the process on exit: whichever process (self-spawned or supervisor-spawned) loses the race for the port just exits without doing any work, so exactly one instance is ever running.

Types:

- `SingleInstance` -- outcome of `claim_single_instance`: `Claimed(TcpListener)` (this process holds the lock; dropping the listener releases it), `AlreadyRunning` (another instance holds it; caller must not do any real work), or `Unavailable` (claim failed for an unrelated reason, e.g. a local permissions/network-stack issue; caller should proceed without the guarantee rather than block startup on it).
- `Release { assets: Vec<Asset> }` -- subset of GitHub's `GET /repos/{owner}/{repo}/releases/latest` response.
- `Asset { name: String, digest: Option<String>, browser_download_url: String }` -- subset of a release asset; `digest` is GitHub-computed (`"sha256:<hex>"`), present on assets uploaded since GitHub added artifact digests.

Constants:

- `CLAIM_ATTEMPTS: u32 = 10`, `CLAIM_RETRY_DELAY: Duration = 100ms` -- retry budget for `claim_single_instance` on `AddrInUse` before concluding another instance is genuinely running, covering the brief window between an updater spawning its replacement and that replacement claiming the port for itself.

Public functions:

- `claim_single_instance(port: u16) -> SingleInstance` -- tries to bind `port` (the caller passes `Config::instance_lock_port`), retrying on `AddrInUse` up to `CLAIM_ATTEMPTS` times (`CLAIM_RETRY_DELAY` apart) before returning `AlreadyRunning`; any other bind error returns `Unavailable` after logging a `warn!`.
- `check_and_update(client: &Client, service_mode: bool, instance_lock: &mut Option<TcpListener>)` (async) -- thin wrapper around `try_update` that logs and swallows any `Err` as a `warn!`; returns `()`, not `Result`, so the type itself documents that this can never fail startup. On a successful update, `try_update` does not return (see below), so this function doesn't return in that case either.

Key internal functions:

- `repository_owner_and_name() -> anyhow::Result<(&'static str, &'static str)>` -- splits Cargo.toml's `package.repository` URL (read via `env!("CARGO_PKG_REPOSITORY")`) into `(owner, name)`, so the GitHub repo checked for updates has a single source of truth instead of separately hardcoded constants.
- `try_update(client: &Client, service_mode: bool, instance_lock: &mut Option<TcpListener>) -> anyhow::Result<()>` (async) -- the full check/update flow:
  1. Resolves `(repo_owner, repo_name)` via `repository_owner_and_name`, and the running executable's path and file name via `std::env::current_exe()`.
  2. Fetches the latest release via the GitHub API and finds the asset whose `name` matches the running exe's file name; logs and returns `Ok(())` if none matches (e.g. no released asset for the current platform).
  3. Strips the `sha256:` prefix from that asset's `digest`; logs and returns `Ok(())` if it has none.
  4. Hashes the running exe's own bytes (`sha2::Sha256` over `std::fs::read` of `current_exe()`) and compares (case-insensitive) to the remote digest; logs and returns `Ok(())` if they match.
  5. On a mismatch, downloads the asset fully into memory, hashes it, and `bail!`s if that hash doesn't match the published digest (refuses to install a corrupted/tampered download).
  6. Writes the verified bytes to a sibling temp path (`exe_path.with_extension("exe.new")`, same directory as the running exe so the replace stays on one filesystem), calls `self_replace::self_replace` on it, then best-effort deletes the temp file.
  7. If `service_mode` is true, logs and calls `std::process::exit(0)` immediately -- no replacement spawned, no lock touched.
  8. Otherwise, spawns a new process at the same exe path with the same CLI args (`std::env::args_os().skip(1)`). If the spawn itself fails, the `?` on it returns an `Err` instead (propagated up to `check_and_update`, logged, and this process keeps running on its current in-memory code, still holding its single-instance lock -- the file on disk is already updated for whenever it's next restarted by any means). Only once `spawn` has succeeded does it call `instance_lock.take()` to release the single-instance port -- the replacement's own claim attempt at its startup tolerates the brief remaining delay before this process fully exits (see `CLAIM_ATTEMPTS`/`CLAIM_RETRY_DELAY`) -- then calls `std::process::exit(0)`; this function does not return past that point.
- `to_hex(bytes: &[u8]) -> String` -- lowercase-hex-encodes bytes to match the format of GitHub's `digest` field for direct string comparison.

Key algorithm / fail-open contract: every fallible step above returns via `anyhow::Result` and `?`/`Context`, but the only caller (`check_and_update`) never propagates an error -- it logs a `warn!` and lets `main.rs` continue starting checks with the current binary regardless of what went wrong (unreachable GitHub, rate limiting, no matching asset, missing digest, corrupted download, no write permission to the install directory, failure to spawn the replacement, ...). Only a fully verified, successfully-installed update causes an exit. Asset matching by the running exe's own file name (rather than a hardcoded `"kuma-remote.exe"`) keeps the logic platform-agnostic without `cfg(windows)` branches. In the default (non-`service_mode`) path, because the exe on disk is already the new version by the time any process is racing for the single-instance port, it's irrelevant which process (self-spawned or supervisor-spawned) wins that race -- exactly one instance of the new version ends up running either way.
