# Functionality

> Module list adapted from the CLAUDE.md template: this project has no `installed.rs`/`cdk_info.rs` (those belong to a different, software-inventory project). Sections below cover this project's actual modules: `main.rs`, `config.rs`, `scheduler.rs`, `checks/ping.rs`, `checks/heartbeat.rs`, `kuma.rs`, `logging.rs`.

## Data Flow

1. `main.rs` parses CLI args, resolves the config path via `resolve_config_path` (the explicit `--config` value, or else the first of `kuma-remote.yaml`/`kuma-config.yaml`/`config.yaml` that exists), then calls `config::Config::load` with it.
2. `config::Config::load` reads the file, deserializes it as StrictYAML into `Config`, normalizes it, and validates it (non-empty, unique `id`s, non-zero `interval`s, `host` present for `ping` checks).
3. If `config.debug` is set, `main.rs` logs every configured check before starting.
4. `main.rs` builds a single shared `reqwest::Client` and calls `scheduler::spawn_all` (passing `config.debug` and `config.report_run_failures` through), which spawns one tokio task per `CheckConfig`.
5. Each task loops: wait for its `interval` tick, run the check for its `mode` (`ping` requires `host` and reports `Up`/`Down` based on the ping result; `heartbeat` always reports `Up` with message `"Heartbeat"`, optionally including a latency if `host` is set and the ping succeeds), translate the result into a `kuma::PushStatus`, and call `kuma::push`. If the run itself errors out (rather than completing with an `Up`/`Down` result), the error is logged and, when `report_run_failures` is set, also pushed to Kuma as a `down` status with the error as `msg`.
6. `kuma::push` builds the final push URL (`status`/`msg`/`ping` query parameters appended to `push_url`), logs it if `debug` is set, sends the GET request, and treats a non-2xx response as an error.
7. `main.rs` blocks on Ctrl-C; on receipt, it aborts all spawned check tasks and exits.

## Configuration

Single StrictYAML file. Path is set via `--config`, or else resolved by `main::resolve_config_path` to the first existing file among `kuma-remote.yaml`, `kuma-config.yaml`, `config.yaml` (falling back to `kuma-remote.yaml` if none exist, so the load error names a sensible file). Schema: a top-level `checks` sequence of mappings, each deserialized into `config::CheckConfig`. `interval` is parsed via `humantime_serde` from strings like `"30s"`/`"5m"`/`"1h"`. Deserialization uses `strict_yaml_rust::serde::from_str`.

The only other configuration source is the `RUST_LOG` environment variable, read by `logging::init` via `tracing_subscriber::EnvFilter`.

## Build and release packaging

No non-default build behavior: standard `cargo build`/`cargo build --release`, no build script, no custom release profile.

## `main.rs`

Purpose: process entry point -- CLI parsing, config load, task orchestration, shutdown.

Types:

- `Cli` -- `clap::Parser` struct; single field `config: Option<PathBuf>` (`-c`/`--config`, no default -- absence triggers the `DEFAULT_CONFIG_CANDIDATES` lookup).

Constants:

- `DEFAULT_CONFIG_CANDIDATES: [&str; 3]` -- `["kuma-remote.yaml", "kuma-config.yaml", "config.yaml"]`, tried in order when `--config` is not given.

Functions:

- `resolve_config_path(explicit: Option<PathBuf>) -> PathBuf` -- returns `explicit` if given; otherwise returns the first of `DEFAULT_CONFIG_CANDIDATES` that exists on disk, or the first candidate (`kuma-remote.yaml`) if none exist.
- `main() -> anyhow::Result<()>` (async, `#[tokio::main]`) -- initializes logging, parses CLI args, resolves the config path via `resolve_config_path`, loads config, logs every check when `config.debug` is set, builds the shared `reqwest::Client` (with a desktop-Chrome `User-Agent` override -- see below), spawns the scheduler with `config.debug` and `config.report_run_failures`, waits on `tokio::signal::ctrl_c()`, then aborts all check task handles before returning.

Key detail: the `reqwest::Client` is built with `.user_agent(...)` set to a real desktop Chrome-on-Windows string instead of reqwest's default `reqwest/x.y.z`. Some reverse proxies / WAFs (e.g. Cloudflare bot protection) block generic HTTP-client user agents while allowing browsers, which otherwise manifests as push requests failing (404/403) even though the same URL works from a browser.

## `config.rs`

Purpose: StrictYAML config schema and validation.

Types:

- `Config { debug: bool, report_run_failures: bool, checks: Vec<CheckConfig> }` -- `debug` uses `#[serde(default)]` (defaults to `false`); `report_run_failures` uses `#[serde(default = "default_report_run_failures")]` (defaults to `true`).
- `CheckConfig { id: String, name: String, mode: CheckMode, host: Option<String>, push_url: String, interval: Duration }` -- `interval` uses `#[serde(with = "humantime_serde")]`; `host` uses `#[serde(default)]` since it's only required for `CheckMode::Ping`.
- `CheckMode` -- enum with variants `Ping` and `Heartbeat` (`#[serde(rename_all = "lowercase")]`, so the YAML scalars `ping`/`heartbeat` map to them). New check modes extend this enum.

Public functions:

- `Config::load(path: &Path) -> anyhow::Result<Config>` -- reads the file, deserializes via `strict_yaml_rust::serde::from_str`, calls `normalize`, then `validate`.

Key internal functions:

- `Config::normalize(&mut self)` -- corrects known copy-paste mistakes that have one unambiguous fix: if a `push_url` contains `?` (Kuma's dashboard shows the push URL with a `?status=up&msg=OK&ping=` example suffix attached, which users sometimes copy verbatim), it's truncated at the `?` and a warning is logged with the check id and original value.
- `Config::validate(&self) -> anyhow::Result<()>` -- rejects an empty `checks` list, duplicate `id`s (via a `HashSet` of seen ids), any `interval` that is zero, and any `CheckMode::Ping` check whose `host` is `None`.
- `default_report_run_failures() -> bool` -- serde default-value function for `Config::report_run_failures`; returns `true`.

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

- `beat_once(host: Option<String>) -> HeartbeatOutcome` (async) -- when `host` is `Some`, calls `ping::ping_once` and keeps the latency only on `PingOutcome::Up`; `PingOutcome::Down` and a propagated `ping_once` error are both treated as "no latency available", not as a failed heartbeat. When `host` is `None`, returns `latency_ms: None` directly without pinging.

Key algorithm: unlike `checks/ping.rs`, this check's `host` is diagnostic, not load-bearing -- its purpose is proving the `kuma-remote` process is running, so a failed ping only drops the latency figure and never turns the heartbeat down.

## `kuma.rs`

Purpose: Uptime Kuma push-monitor HTTP client.

Types:

- `PushStatus` -- `Up { ping_ms: Option<f64>, message: Option<String> }` (message defaults to `"OK"` when `None`, sent as `"Heartbeat"` by `CheckMode::Heartbeat`) or `Down { message: String }`.

Constants:

- `MAX_MESSAGE_LEN: usize = 250` -- Kuma's approximate cap on the `msg` query parameter; longer messages are truncated before sending.

Public functions:

- `push(client: &Client, push_url: &str, status: PushStatus, debug: bool) -> anyhow::Result<()>` (async) -- parses `push_url` into a `reqwest::Url`, appends `status`/`msg` (`msg` defaults to `"OK"` for `PushStatus::Up` when its `message` is `None`) and `ping` when known as query pairs, logs the final URL when `debug` is set, sends the GET request, and errors on a non-2xx response.

Key internal functions:

- `truncate(s: &str, max_len: usize) -> String` -- truncates on a `char_indices` boundary (never splits a multi-byte UTF-8 character) to at most `max_len` characters.

Note: the URL is built explicitly via `Url::query_pairs_mut()` rather than `RequestBuilder::query()` so the exact final URL string is available to log before the request is sent (`RequestBuilder` doesn't expose its built URL without consuming itself).

## `logging.rs`

Purpose: process-wide `tracing` subscriber setup.

Public functions:

- `init()` -- installs a `tracing_subscriber::fmt` subscriber using `EnvFilter` from `RUST_LOG`, defaulting to `info` if unset or invalid.
