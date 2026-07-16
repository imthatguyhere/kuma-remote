# Functionality

> Module list adapted from the CLAUDE.md template: this project has no `installed.rs`/`cdk_info.rs` (those belong to a different, software-inventory project). Sections below cover this project's actual modules: `main.rs`, `config.rs`, `scheduler.rs`, `checks/ping.rs`, `checks/heartbeat.rs`, `kuma.rs`, `logging.rs`, `updater.rs`.

## Data Flow

1. `main.rs` initializes logging, then immediately logs the app name, version, and author(s) (via `CARGO_PKG_NAME`/`CARGO_PKG_VERSION`/`CARGO_PKG_AUTHORS`), before any config is loaded.
2. `main.rs` parses CLI args, resolves the config path via `resolve_config_path` (the explicit `--config` value, or else the first of `kuma-remote.yaml`/`kuma-config.yaml`/`config.yaml` that exists), then calls `config::Config::load` with it.
3. `config::Config::load` reads the file, deserializes it as StrictYAML into `Config`, normalizes it, and validates it (non-empty, unique `id`s, non-zero `interval`s, `host` present for `ping` checks).
4. If `cli.stop` (`--stop`) is set, `main.rs` calls `handle_stop` and returns its result immediately — nothing below this point runs; no lock is claimed, no checks start.
5. Unless `config.service_mode` is true or `config.instance_lock` is false, `main.rs` runs `updater::claim_single_instance(config.instance_lock_port)` on the blocking thread pool (via `tokio::task::spawn_blocking`, since it does blocking socket I/O and sleeps). If another instance already holds the lock, it logs a warning and returns immediately — no checks start, and no debug logging of check details happens either. Otherwise it holds onto the result (a claimed `InstanceLock` plus a shutdown notifier, or `None`/`None` if the claim was skipped or disabled) for the rest of the process; if the claim instead came back `Unavailable`, it also sets a `lock_unavailable` flag (also `None`/`None`, but remembered as an anomaly rather than a deliberate choice — see `updater.rs`).
6. If `config.debug` is set, `main.rs` logs every configured check — deliberately after the single-instance claim above, so a duplicate instance never logs check details (including `push_url`, a bearer credential) before exiting.
7. `main.rs` builds a single shared `reqwest::Client` with a connect timeout and an overall request timeout (so a stalled GitHub/Kuma response can never hang startup or a push indefinitely; the release-asset download in `updater.rs` bounds itself separately, see below). If `config.auto_update` is set, it then calls `updater::check_and_update` with that client, `config.service_mode`, the `lock_unavailable` flag, the single-instance lock, and `config.slow_download_mode` before doing anything else — see `updater.rs` below. If the result is `UpdateOutcome::Exit` (an update was applied; self-spawning a replacement only when the single-instance lock is actually held, otherwise — including under `service_mode` — relying on a supervisor to restart it), `main.rs` returns `Ok(())` immediately, so the process exits via its own normal return path; on `UpdateOutcome::Continue` it falls through to normal startup regardless of what happened during the check.
8. `main.rs` calls `scheduler::spawn_all` (passing `config.debug` and `config.report_run_failures` through), which spawns one tokio task per `CheckConfig`.
9. Each task loops: wait for its `interval` tick, run the check for its `mode` (`ping` requires `host` and reports `Up`/`Down` based on the ping result; `heartbeat` always reports `Up` with message `"Heartbeat"`, optionally including a latency if `host` is set and the ping succeeds), translate the result into a `kuma::PushStatus`, and call `kuma::push`. If the run itself errors out (rather than completing with an `Up`/`Down` result), the error is logged and, when `report_run_failures` is set, also pushed to Kuma as a `down` status with the error as `msg`.
10. `kuma::push` builds the final push URL (`status`/`msg`/`ping` query parameters appended to `push_url`), logs it if `debug` is set, sends the GET request, and treats a non-2xx response as an error.
11. `main.rs` waits on a `tokio::select!` between `tokio::signal::ctrl_c()` and (when the single-instance lock is held) the shutdown notifier being triggered by a `--stop` request handled on `updater.rs`'s handshake responder thread; on either, it aborts all spawned check tasks and exits. When the lock isn't held, it waits on Ctrl-C alone (there is no listener for a `--stop` client to connect to anyway).

## Configuration

Single StrictYAML file. Path is set via `--config`, or else resolved by `main::resolve_config_path` to the first existing file among `kuma-remote.yaml`, `kuma-config.yaml`, `config.yaml` (falling back to `kuma-remote.yaml` if none exist, so the load error names a sensible file). Schema: a top-level `checks` sequence of mappings, each deserialized into `config::CheckConfig`. `interval` is parsed via `humantime_serde` from strings like `"30s"`/`"5m"`/`"1h"`. Deserialization uses `strict_yaml_rust::serde::from_str`. `auto_update` (`bool`, default `true` via `default_auto_update`) gates whether `updater::check_and_update` runs at all; `service_mode` (`bool`, default `false`) and `instance_lock`/`instance_lock_port` (`bool`/`u16`, defaults `true`/`51247` via `default_instance_lock`/`default_instance_lock_port`) control the single-instance guard and the updater's restart strategy; `slow_download_mode` (`bool`, default `false` via `#[serde(default)]`) controls how the update download is time-bounded — see `updater.rs`. `Config::validate` rejects `instance_lock_port == 0` while the lock is actually in effect (`instance_lock` true and `service_mode` false) as a hard startup error (not just a warning): port `0` always binds to a fresh OS-assigned port on every attempt, so it can never detect a duplicate instance, silently defeating the lock rather than just weakening it.

The only other configuration source is the `RUST_LOG` environment variable, read by `logging::init` via `tracing_subscriber::EnvFilter`.

## Build and release packaging

No non-default build behavior beyond `build.rs` (Windows icon resource via `winresource`) and the `[profile.release]` tuning in `Cargo.toml`. There is no CI/release pipeline; `scripts/build-release.ps1` builds and copies the exe into `dist/` for manual upload to a GitHub release. `updater.rs` relies on GitHub computing a SHA-256 `digest` for each uploaded release asset itself (returned by the releases API) — no project-side checksum file is generated or required.

## `main.rs`

Purpose: process entry point — CLI parsing, config load, task orchestration, shutdown.

Types:

- `Cli` — `clap::Parser` struct; `config: Option<PathBuf>` (`-c`/`--config`, no default — absence triggers the `DEFAULT_CONFIG_CANDIDATES` lookup) and `stop: bool` (`--stop`, no short form).

Constants:

- `DEFAULT_CONFIG_CANDIDATES: [&str; 3]` — `["kuma-remote.yaml", "kuma-config.yaml", "config.yaml"]`, tried in order when `--config` is not given.

Functions:

- `resolve_config_path(explicit: Option<PathBuf>) -> PathBuf` — returns `explicit` if given; otherwise returns the first of `DEFAULT_CONFIG_CANDIDATES` that exists on disk, or the first candidate (`kuma-remote.yaml`) if none exist.
- `main() -> anyhow::Result<()>` (async, `#[tokio::main]`) — initializes logging, logs the app name/version/authors (`env!("CARGO_PKG_NAME"/"CARGO_PKG_VERSION"/"CARGO_PKG_AUTHORS")`), parses CLI args, resolves the config path via `resolve_config_path`, loads config. If `cli.stop`, calls `handle_stop` and returns its result immediately. Otherwise: unless `config.service_mode` is true or `config.instance_lock` is false, runs `updater::claim_single_instance(config.instance_lock_port)` via `tokio::task::spawn_blocking` and returns immediately (logging a warning) if another instance already holds the lock — before any debug logging of check details; destructures a successful `SingleInstance::Claimed { lock, stop_requested }` into `(Some(lock), Some(stop_requested))`, and sets a local `lock_unavailable` flag if the claim instead came back `Unavailable`. Logs every check when `config.debug` is set. Builds the shared `reqwest::Client` (with a desktop-Chrome `User-Agent` override and connect/request timeouts — see below), runs `updater::check_and_update` (passing `config.service_mode`, `lock_unavailable`, the single-instance lock, and `config.slow_download_mode` through) when `config.auto_update` is set and returns `Ok(())` immediately if it reports `UpdateOutcome::Exit`, spawns the scheduler with `config.debug` and `config.report_run_failures`, then waits on a `tokio::select!` between `tokio::signal::ctrl_c()` and (only when `stop_requested` is `Some`) `stop_requested.notified()`, before aborting all check task handles and returning.
- `handle_stop(config: &config::Config) -> anyhow::Result<()>` (async) — handles `--stop`. `bail!`s with a explanatory message if `config.service_mode` is true (no lock/control channel exists in that mode) or `config.instance_lock` is false (same reason). Otherwise calls `updater::request_stop(config.instance_lock_port)`; logs and returns `Ok(())` if it returns `Ok(true)` (acknowledged), `bail!`s naming the port if it returns `Ok(false)` (nothing listening), and propagates any `Err` via `.context("Requesting stop")`.

Key detail: the `reqwest::Client` is built with `.user_agent(...)` set to a real desktop Chrome-on-Windows string instead of reqwest's default `reqwest/x.y.z`. Some reverse proxies / WAFs (e.g. Cloudflare bot protection) block generic HTTP-client user agents while allowing browsers, which otherwise manifests as push requests failing (404/403) even though the same URL works from a browser. It also sets a 7s connect timeout and a 30s overall request timeout (`.connect_timeout`/`.timeout`), so a stalled response from GitHub's API or Kuma itself can never hang a caller (notably `updater::check_and_update`, which runs before any check task is spawned) indefinitely; the release-asset download in `updater.rs` doesn't use this client-level timeout at all, instead bounding itself per-chunk in `download_with_progress` (see `DOWNLOAD_HARD_TIMEOUT`/`DOWNLOAD_STALL_TIMEOUT`), relying on periodic progress logging in the meantime rather than a single short deadline.

## `config.rs`

Purpose: StrictYAML config schema and validation.

Types:

- `Config { debug: bool, report_run_failures: bool, auto_update: bool, service_mode: bool, instance_lock: bool, instance_lock_port: u16, slow_download_mode: bool, checks: Vec<CheckConfig> }` — `debug`, `service_mode`, and `slow_download_mode` use `#[serde(default)]` (default `false`); `report_run_failures` uses `#[serde(default = "default_report_run_failures")]` (defaults to `true`); `auto_update` uses `#[serde(default = "default_auto_update")]` (defaults to `true`); `instance_lock` uses `#[serde(default = "default_instance_lock")]` (defaults to `true`); `instance_lock_port` uses `#[serde(default = "default_instance_lock_port")]` (defaults to `51247`).
- `CheckConfig { id: String, name: String, mode: CheckMode, host: Option<String>, push_url: String, interval: Duration }` — `interval` uses `#[serde(with = "humantime_serde")]`; `host` uses `#[serde(default)]` since it's only required for `CheckMode::Ping`.
- `CheckMode` — enum with variants `Ping` and `Heartbeat` (`#[serde(rename_all = "lowercase")]`, so the YAML scalars `ping`/`heartbeat` map to them). New check modes extend this enum.

Public functions:

- `Config::load(path: &Path) -> anyhow::Result<Config>` — reads the file, deserializes via `strict_yaml_rust::serde::from_str`, calls `normalize`, then `validate`.

Key internal functions:

- `Config::normalize(&mut self)` — corrects known copy-paste mistakes that have one unambiguous fix: if a `push_url` contains `?` (Kuma's dashboard shows the push URL with a `?status=up&msg=OK&ping=` example suffix attached, which users sometimes copy verbatim), it's truncated at the `?` and a warning is logged with the check id and original value.
- `Config::validate(&self) -> anyhow::Result<()>` — rejects an empty `checks` list, duplicate `id`s (via a `HashSet` of seen ids), any `interval` that is zero, any `CheckMode::Ping` check whose `host` is `None`, and an `instance_lock_port` of `0` while the single-instance lock is actually in effect (`instance_lock` true and `service_mode` false) — a `0` port always binds to a fresh OS-assigned port on every attempt, so it can never detect a duplicate instance, which would silently defeat the lock rather than just weaken it, hence a hard error instead of a warning.
- `default_report_run_failures() -> bool` — serde default-value function for `Config::report_run_failures`; returns `true`.
- `default_auto_update() -> bool` — serde default-value function for `Config::auto_update`; returns `true`.
- `default_instance_lock() -> bool` — serde default-value function for `Config::instance_lock`; returns `true`.
- `default_instance_lock_port() -> u16` — serde default-value function for `Config::instance_lock_port`; returns `51247`.

## `scheduler.rs`

Purpose: runs each check on its own independent interval and drives the check -> push pipeline.

Public functions:

- `spawn_all(checks: Vec<CheckConfig>, client: Client, debug: bool, report_run_failures: bool) -> Vec<JoinHandle<()>>` — spawns one `tokio::spawn`ed task per check (each with its own cloned `Client`) and returns their handles. `debug` and `report_run_failures` are threaded through to every check loop.

Key internal functions:

- `run_check_loop(check: CheckConfig, client: Client, debug: bool, report_run_failures: bool)` — owns a `tokio::time::interval(check.interval)` (with `MissedTickBehavior::Delay`, so a slow check run doesn't cause a burst of catch-up ticks) and calls `run_once` on every tick, logging (not propagating) any error so one bad run doesn't kill the loop. When `report_run_failures` is set, a run error is also pushed to Kuma as `PushStatus::Down { message: "Check run failed: {err}" }`; a failure to push that is itself logged (and swallowed).
- `run_once(check: &CheckConfig, client: &Client, debug: bool) -> anyhow::Result<()>` — matches on `check.mode`: `CheckMode::Ping` unwraps `check.host` (present per `Config::validate`), runs `ping::ping_once`, and maps its `Up`/`Down` outcome straight to `PushStatus::Up { ping_ms: Some(_), message: None }` / `PushStatus::Down`; `CheckMode::Heartbeat` calls `heartbeat::beat_once(check.host.clone())` and always builds `PushStatus::Up { ping_ms: <from the outcome>, message: Some("Heartbeat") }`. Either way, the outcome is logged and the resulting `kuma::PushStatus` is pushed.

Algorithm: per-check tasks are fully independent (`tokio::spawn` per check, not a shared single loop), so no check's schedule or latency affects any other check's timing.

## `checks/ping.rs`

Purpose: the `ping` check mode — a single ICMP echo per run.

Types:

- `PingOutcome` — `Up { latency_ms: f64 }` or `Down { reason: String }`.

Constants:

- `RECV_TIMEOUT: Duration = 5s` — upper bound on how long `ping_once` waits for a result from the `pinger` channel, independent of any OS-level ping timeout.

Public functions:

- `ping_once(host: String) -> anyhow::Result<PingOutcome>` (async) — runs the blocking ping logic on a `tokio::task::spawn_blocking` thread and awaits it.

Key internal functions:

- `ping_once_blocking(host: &str) -> anyhow::Result<PingOutcome>` — builds `pinger::PingOptions`, starts `pinger::ping`, and classifies the first `PingResult` received (`Pong` -> `Up`, `Timeout`/`Unknown`/`PingExited` -> `Down` with a reason), or `Down` on a `recv_timeout` expiry.

Key algorithm / platform behavior: `pinger` shells out to the OS `ping` binary on Unix (or uses the native `IcmpSendEcho` API via `winping` on Windows, in-process, no subprocess). On Unix, plain `ping` streams echoes forever, so taking one result and dropping the channel would otherwise leave the child `ping` process and its reader thread running forever. To prevent that leak, Linux builds add `-c 1 -W 2` and BSD-family builds (`macos`/`freebsd`/`openbsd`/`netbsd`/`dragonfly`) add `-c 1 -t 2` (BSD ping's `-t` is a wait-timeout, not TTL) via `PingOptions::with_raw_arguments`, so the process always exits after one probe. Windows needs no such flag: `winping`'s `IcmpSendEcho` call has its own built-in 2s timeout and never spawns a subprocess.

## `checks/heartbeat.rs`

Purpose: the `heartbeat` check mode — always reports the process is alive, with an optional ping-derived latency.

Types:

- `HeartbeatOutcome { latency_ms: Option<f64> }` — always maps to `Up` in `scheduler::run_once`; `latency_ms` is `None` when no `host` was configured, or when the ping to it failed or timed out.

Public functions:

- `beat_once(host: Option<String>) -> HeartbeatOutcome` (async) — when `host` is `Some`, calls `ping::ping_once` and keeps the latency only on `PingOutcome::Up`; `PingOutcome::Down` and a propagated `ping_once` error are both treated as "no latency available", not as a failed heartbeat, and are logged via `tracing::warn!` (with the host and reason/error) so a misconfigured or unreachable diagnostic host is still visible in logs. When `host` is `None`, returns `latency_ms: None` directly without pinging.

Key algorithm: unlike `checks/ping.rs`, this check's `host` is diagnostic, not load-bearing — its purpose is proving the `kuma-remote` process is running, so a failed ping only drops the latency figure and never turns the heartbeat down.

## `kuma.rs`

Purpose: Uptime Kuma push-monitor HTTP client.

Types:

- `PushStatus` — `Up { ping_ms: Option<f64>, message: Option<String> }` (message defaults to `"OK"` when `None`, sent as `"Heartbeat"` by `CheckMode::Heartbeat`) or `Down { message: String }`.

Constants:

- `MAX_MESSAGE_LEN: usize = 250` — Kuma's approximate cap on the `msg` query parameter; longer messages are truncated before sending.

Public functions:

- `push(client: &Client, push_url: &str, status: PushStatus, debug: bool) -> anyhow::Result<()>` (async) — parses `push_url` into a `reqwest::Url`, appends `status`/`msg` (`msg` defaults to `"OK"` for `PushStatus::Up` when its `message` is `None`, and is truncated via `truncate` for both `Up` and `Down`) and `ping` when known as query pairs, logs the final URL when `debug` is set, sends the GET request, and errors on a non-2xx response.

Key internal functions:

- `truncate(s: &str, max_len: usize) -> String` — truncates on a `char_indices` boundary (never splits a multi-byte UTF-8 character) to at most `max_len` characters.

Note: the URL is built explicitly via `Url::query_pairs_mut()` rather than `RequestBuilder::query()` so the exact final URL string is available to log before the request is sent (`RequestBuilder` doesn't expose its built URL without consuming itself).

## `logging.rs`

Purpose: process-wide `tracing` subscriber setup.

Public functions:

- `init()` — installs a `tracing_subscriber::fmt` subscriber using `EnvFilter` from `RUST_LOG`, defaulting to `info` if unset or invalid.

## `updater.rs`

Purpose: startup self-updater — checks GitHub's latest release for a newer build of the running executable, replaces it in place when found, and either spawns a replacement process running the new binary and exits, or just exits and trusts a process supervisor to restart it. Self-spawning only happens while this process actually holds the single-instance lock (a configurable loopback TCP port): that's what makes it safe even if a process supervisor *also* restarts the process on exit (whichever process, self-spawned or supervisor-spawned, loses the race for the port just exits without doing any work, so exactly one instance is ever running). `service_mode` always takes the exit-only path (no self-spawn, no lock ever claimed); outside `service_mode`, the exit-only path is also used as a fallback whenever the lock isn't held (`instance_lock: false`, or a claim that came back `Unavailable`), since self-spawning without the lock's protection would risk an unprotected duplicate instance. The same lock port also serves as a graceful-shutdown channel for `kuma-remote --stop`, since a self-spawned replacement is a detached process a terminal's Ctrl-C can't always reach.

Types:

- `InstanceLock { listener: TcpListener, port: u16, shutdown: Arc<AtomicBool> }` — owns a claimed lock: `listener` is held only so its own `Drop` closes this process's socket handle (never read directly, `#[allow(dead_code)]`); dropping an `InstanceLock` (including via `Option::take`) sets `shutdown` and connects once to `port` to wake the handshake responder thread's blocking `accept()`, so it notices the flag and closes its own cloned socket too — both handles closed is what actually frees the OS-level port for a replacement process to claim, rather than leaving it bound until this whole process exits.
- `SingleInstance` — outcome of `claim_single_instance`: `Claimed { lock: InstanceLock, stop_requested: Arc<Notify> }` (this process holds the lock; `stop_requested` is notified by the handshake responder thread the moment a valid `--stop` request comes in, for the caller to race against `tokio::signal::ctrl_c()`), `AlreadyRunning` (another instance holds it; caller must not do any real work), or `Unavailable` (claim failed for a reason other than a genuine duplicate instance — a local permissions/network-stack issue, or the port being held by an unrelated, non-kuma-remote process; caller should proceed without the guarantee rather than block startup on it).
- `UpdateOutcome` — outcome of `check_and_update`/`try_update`: `Continue` (no update applied; proceed with startup) or `Exit` (an update was applied on disk; the caller should stop startup and return, so the process exits via `main`'s own return path rather than a `std::process::exit` call buried inside this module).
- `Release { assets: Vec<Asset> }` — subset of GitHub's `GET /repos/{owner}/{repo}/releases/latest` response.
- `Asset { name: String, digest: Option<String>, browser_download_url: String }` — subset of a release asset; `digest` is GitHub-computed (`"sha256:<hex>"`), present on assets uploaded since GitHub added artifact digests.

Constants:

- `CLAIM_ATTEMPTS: u32 = 10`, `CLAIM_RETRY_DELAY: Duration = 100ms` — retry budget for `claim_single_instance` on `AddrInUse` before concluding another instance may be genuinely running, covering the brief window between an updater spawning its replacement and that replacement claiming the port for itself.
- `OCCUPANT_CLASSIFY_ATTEMPTS: u32 = 2` — how many times `classify_occupant` retries an ambiguous identity-handshake result (timeout or connection error) before concluding the occupant isn't kuma-remote; a definite mismatch (wrong bytes received) is trusted immediately since a retry can't change that.
- `LOCK_MAGIC: &[u8]` — fixed byte string the lock holder writes back to any connection on the lock port, letting a challenger confirm the occupant is genuinely another kuma-remote instance rather than an unrelated process/service that happens to be bound to the same port.
- `STOP_COMMAND: &[u8]`, `STOP_ACK: &[u8]` — exact byte strings exchanged after the `LOCK_MAGIC` handshake to request and acknowledge a graceful shutdown (see `request_stop`/`spawn_handshake_responder`). Requiring an exact match keeps an incidental local connection (e.g. a port scanner, or `classify_occupant`'s own probe, which never writes anything back) from ever being mistaken for a real stop request.
- `STOP_COMMAND_TIMEOUT: Duration = 2s` — how long the handshake responder waits, after writing `LOCK_MAGIC`, for `STOP_COMMAND` before giving up on that connection; also the budget `request_stop` allows for the acknowledgement that follows.
- `HANDSHAKE_TIMEOUT: Duration = 5s` — connect/read timeout used when a challenger (`classify_occupant` or `request_stop`) probes the occupant of the lock port for `LOCK_MAGIC`. Generous relative to the handshake responder's own work so a momentarily busy host (e.g. antivirus scanning the two executables involved in a self-update) doesn't cause a genuine occupant to be misclassified as unrelated.
- `SPAWN_ATTEMPTS: u32 = 3`, `SPAWN_RETRY_DELAY: Duration = 250ms` — retry budget for spawning the replacement process in `try_update`, covering a transient failure to launch the just-replaced executable (e.g. a brief antivirus scan lock). `SPAWN_RETRY_DELAY` also doubles as the grace period given to a freshly spawned replacement before confirming it hasn't already exited.
- `DOWNLOAD_HARD_TIMEOUT: Duration = 5 minutes` — when `Config::slow_download_mode` is off (the default), the ceiling on the *total* time the release-asset download may take, regardless of progress; fails fast on a connection that's technically up but impractically slow.
- `DOWNLOAD_STALL_TIMEOUT: Duration = 60s` — when `Config::slow_download_mode` is on, the ceiling on how long a *single* chunk read may take before the download is considered stalled and aborted; unlike `DOWNLOAD_HARD_TIMEOUT` this doesn't cap the overall download.
- `DOWNLOAD_PROGRESS_INTERVAL: Duration = 5s` — how often `download_with_progress` logs download progress.
- `MAX_DOWNLOAD_SIZE: u64 = 200 MiB` — safety cap on the release asset, checked against `Content-Length` up front and against bytes actually received as they arrive (in case that header is absent or wrong); guards against a misconfigured or unexpectedly huge asset causing an unbounded in-memory allocation.

Public functions:

- `claim_single_instance(port: u16) -> SingleInstance` — tries to bind `port` (the caller passes `Config::instance_lock_port`), retrying on `AddrInUse` up to `CLAIM_ATTEMPTS` times (`CLAIM_RETRY_DELAY` apart). On success, creates the `stop_requested` notifier and a `shutdown` flag, spawns a handshake responder (see `spawn_handshake_responder`) sharing both, and returns `Claimed { lock: InstanceLock { listener, port, shutdown }, stop_requested }`. If every retry sees `AddrInUse`, calls `classify_occupant` to distinguish a genuine duplicate from an unrelated occupant instead of assuming the former. Any other bind error returns `Unavailable` after logging a `warn!`. Runs blocking I/O throughout, so callers on a tokio runtime invoke it via `tokio::task::spawn_blocking` rather than calling it directly from an async context (see `main.rs`).
- `check_and_update(client: &Client, service_mode: bool, lock_unavailable: bool, instance_lock: &mut Option<InstanceLock>, slow_download_mode: bool) -> UpdateOutcome` (async) — thin wrapper around `try_update` that logs and swallows any `Err` as a `warn!`, returning `UpdateOutcome::Continue` in that case so the type itself documents that this can never fail startup. Distinguishes a connect/request timeout (checked via `reqwest::Error::is_timeout` on the error chain) from any other failure, logging `"Update check failed: Timeout"` for the former and the generic swallowed-error message for the latter. `lock_unavailable` (set by `main.rs` when the initial claim came back `SingleInstance::Unavailable`) lets `try_update` log the exit-only fallback as an `error!` rather than a routine notice when the lock genuinely couldn't be claimed, as opposed to `instance_lock: false` being deliberately configured. On `Ok`, passes `try_update`'s `UpdateOutcome` straight through to the caller (`main.rs`), which is responsible for actually exiting on `Exit` — neither this function nor `try_update` calls `std::process::exit` itself, so the exit always happens via `main`'s own return path.
- `request_stop(port: u16) -> anyhow::Result<bool>` (async) — the client side of the graceful-shutdown channel, called by `main::handle_stop` for `--stop`. Connects to `port` within `HANDSHAKE_TIMEOUT` (`bail!`s on a timeout); `Ok(false)` if the connection is refused (nothing listening — no running instance to stop). Otherwise reads and checks `LOCK_MAGIC` within `HANDSHAKE_TIMEOUT` (`bail!`s if it doesn't match, refusing to send a stop command to a non-kuma-remote occupant), sends `STOP_COMMAND`, and reads the `STOP_ACK` reply within `STOP_COMMAND_TIMEOUT`, returning whether it matched.

Key internal functions:

- `spawn_handshake_responder(listener: &TcpListener, stop_requested: Arc<Notify>, shutdown: Arc<AtomicBool>)` — clones `listener` and spawns a detached OS thread that, for every incoming connection: first checks `shutdown` and breaks (closing its cloned socket) if set, otherwise writes `LOCK_MAGIC`, then waits up to `STOP_COMMAND_TIMEOUT` for the connecting side to send exactly `STOP_COMMAND` in reply; if it does, writes `STOP_ACK` and calls `stop_requested.notify_one()`. A connection that never sends `STOP_COMMAND` (including `classify_occupant`'s bare identity probe) just times out and the loop moves to the next connection. Exits once `shutdown` is set and the next connection (typically `InstanceLock::drop`'s own wake-up connection) arrives; best-effort (a clone failure just means a later challenger treats this occupant as unidentifiable rather than confirmed, and `--stop` has nothing to connect to).
- `classify_occupant(port: u16) -> SingleInstance` — called once `claim_single_instance`'s bind retries are exhausted: connects to `port` with `HANDSHAKE_TIMEOUT` and checks the response for `LOCK_MAGIC`, retrying up to `OCCUPANT_CLASSIFY_ATTEMPTS` times on an ambiguous result (connect/read timeout or error) before giving up. A match returns `AlreadyRunning` (genuine duplicate); a definite mismatch (wrong bytes, no retry) or an ambiguous result exhausted across all attempts returns `Unavailable` after a `warn!`, since the port is evidently held by something other than kuma-remote (or couldn't be confirmed as such).
- `repository_owner_and_name() -> anyhow::Result<(&'static str, &'static str)>` — splits Cargo.toml's `package.repository` URL (read via `env!("CARGO_PKG_REPOSITORY")`) into `(owner, name)`, so the GitHub repo checked for updates has a single source of truth instead of separately hardcoded constants.
- `download_with_progress(response: Response, slow_download_mode: bool) -> anyhow::Result<(Vec<u8>, String)>` (async) — streams `response`'s body to completion via repeated `Response::chunk()` calls, hashing it incrementally with `sha2::Sha256` (no separate full-buffer digest pass needed afterward) and logging progress every `DOWNLOAD_PROGRESS_INTERVAL`. Each chunk read is wrapped in `tokio::time::timeout`: when `slow_download_mode` is off, the timeout is the remaining time until `DOWNLOAD_HARD_TIMEOUT` since the download started (so the *total* download is capped); when on, it's a flat `DOWNLOAD_STALL_TIMEOUT` per chunk (so only a stretch with zero progress aborts it). Enforces `MAX_DOWNLOAD_SIZE` against bytes actually received as they arrive, in case `Content-Length` was absent or wrong. Returns the body bytes and their lowercase-hex SHA-256 digest.
- `try_update(client: &Client, service_mode: bool, lock_unavailable: bool, instance_lock: &mut Option<InstanceLock>, slow_download_mode: bool) -> anyhow::Result<UpdateOutcome>` (async) — the full check/update flow:
  1. Resolves `(repo_owner, repo_name)` via `repository_owner_and_name`, and the running executable's path and file name via `std::env::current_exe()`.
  2. Fetches the latest release via the GitHub API and finds the asset whose `name` matches the running exe's file name; logs and returns `Ok(UpdateOutcome::Continue)` if none matches (e.g. no released asset for the current platform).
  3. Strips the `sha256:` prefix from that asset's `digest`; logs and returns `Ok(UpdateOutcome::Continue)` if it has none.
  4. Reads the running exe's own bytes (`tokio::fs::read` of `current_exe()`) and hashes them on a blocking thread (`tokio::task::spawn_blocking` wrapping `sha2::Sha256`, since hashing a multi-MB file is CPU-bound work that shouldn't run directly on an async worker), then compares (case-insensitive) to the remote digest; logs and returns `Ok(UpdateOutcome::Continue)` if they match.
  5. On a mismatch, requests the asset, checks `Content-Length` against `MAX_DOWNLOAD_SIZE` up front if present, then streams and hashes it via `download_with_progress(response, slow_download_mode)`, and `bail!`s if that hash doesn't match the published digest (refuses to install a corrupted/tampered download).
  6. Writes the verified bytes to a sibling temp path (`exe_path.with_extension("exe.new")`, same directory as the running exe so the replace stays on one filesystem, via `tokio::fs::write`), calls `self_replace::self_replace` on it, then best-effort deletes the temp file (`tokio::fs::remove_file`) on either outcome — success or failure — rather than only after success, so a failed replace doesn't leak the downloaded temp file on disk.
  7. If `service_mode` is true, or `instance_lock` is `None` (lock disabled or unavailable), logs and returns `Ok(UpdateOutcome::Exit)` immediately — no replacement spawned, no lock touched, no `std::process::exit` call. Logs at `error!` if `lock_unavailable` is set (the lock claim genuinely failed — an anomaly worth investigating, since nothing will restart this process without a supervisor), or `info!` if the operator simply configured `instance_lock: false`.
  8. Otherwise (lock held), spawns a new process at the same exe path with the same CLI args (`std::env::args_os().skip(1)`) via `tokio::task::spawn_blocking` wrapping `std::process::Command::spawn` (the underlying OS spawn call can itself block on a busy host, e.g. antivirus scanning the just-written exe, so it's offloaded like the hashing above rather than run directly on an async worker), retrying up to `SPAWN_ATTEMPTS` times (`SPAWN_RETRY_DELAY` apart) if the spawn itself fails. After each successful spawn, waits `SPAWN_RETRY_DELAY` and calls `Child::try_wait` to confirm the replacement hasn't already exited; both an already-exited replacement and a `try_wait` error (status genuinely couldn't be confirmed either way) are treated as failed and retried, rather than assuming the latter means success. If every attempt fails to produce a still-running replacement, logs an `error!` (distinct from the routine `warn!` skip cases, since this means an already-verified update is stuck) and `bail!`s (propagated up to `check_and_update`; this process keeps running on its current in-memory code, still holding its single-instance lock — the file on disk is already updated for whenever it's next restarted by any means). Only once a replacement is confirmed still running does it call `instance_lock.take()`, dropping the `InstanceLock` to actually release the single-instance port (see the `InstanceLock`/`spawn_handshake_responder` entries above) — the replacement's own claim attempt at its startup tolerates the brief remaining delay before that release completes (see `CLAIM_ATTEMPTS`/`CLAIM_RETRY_DELAY`) — then returns `Ok(UpdateOutcome::Exit)`.
- `to_hex(bytes: &[u8]) -> String` — lowercase-hex-encodes bytes to match the format of GitHub's `digest` field for direct string comparison.

Key algorithm / fail-open contract: every fallible step above returns via `anyhow::Result` and `?`/`Context`, but the only caller (`check_and_update`) never propagates an error — it logs a `warn!` and returns `UpdateOutcome::Continue`, letting `main.rs` continue starting checks with the current binary regardless of what went wrong (unreachable GitHub, rate limiting, no matching asset, missing digest, corrupted download, no write permission to the install directory, repeated failure to spawn a replacement that stays running, ...). A stalled (rather than failed) GitHub/CDN connection is bounded by the shared `reqwest::Client`'s connect timeout (set in `main.rs`); the release-asset download is instead bounded by `download_with_progress`'s own per-chunk timeout (`DOWNLOAD_HARD_TIMEOUT` total, or `DOWNLOAD_STALL_TIMEOUT` per chunk under `slow_download_mode`), relying on periodic progress logging in the meantime rather than a single short deadline. Only a fully verified, successfully-installed, successfully-restarted (or intentionally exit-only) update returns `UpdateOutcome::Exit`; neither `try_update` nor `check_and_update` calls `std::process::exit` — `main.rs` is the sole place that actually returns/exits on that outcome, so the exit is always a normal return through `main`'s own control flow rather than a hard exit buried in this module. Asset matching by the running exe's own file name (rather than a hardcoded `"kuma-remote.exe"`) keeps the logic platform-agnostic without `cfg(windows)` branches. Self-spawning is gated on actually holding the single-instance lock (not merely on `service_mode` being false), so disabling the lock via `instance_lock: false` falls back to the same exit-only, supervisor-dependent behavior as `service_mode: true` rather than silently self-spawning unprotected. The `LOCK_MAGIC` handshake means a port collision with an unrelated (non-kuma-remote) process is treated as `Unavailable` (proceed without the guarantee) rather than misclassified as `AlreadyRunning` (which would otherwise exit without starting any checks); an ambiguous (timed-out) handshake is retried once (`OCCUPANT_CLASSIFY_ATTEMPTS`) before falling back to `Unavailable`, since a momentarily slow genuine occupant would otherwise look identical to an unrelated process on a single attempt.
