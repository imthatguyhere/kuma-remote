# kuma-remote

[![Static Badge](https://img.shields.io/badge/License-PolyForm_Noncommercial_License_1.0.0-582aad)](LICENSE.md)

Kuma Remote is a console client for [Uptime Kuma](https://github.com/louislam/uptime-kuma) push monitors.
It runs a set of locally-defined checks against hosts on your network and reports each result (up/down, latency, message) to that check's Uptime Kuma push URL, so hosts that can't be reached directly by your Kuma server can still be monitored from the machine that runs `kuma-remote`.

It runs as a long-lived daemon: each configured check runs independently on its own interval for as long as the process is alive.

---

## Prerequisites

- Rust 1.94 or later (edition 2024) to build from source.
- Network access from the machine running `kuma-remote` to both the checked hosts and the Uptime Kuma push URL.
- No administrator/root privileges are required. On Windows, pings use the native `IcmpSendEcho` API; on Linux/macOS, the OS `ping` binary is invoked directly (no raw-socket capability needed).
- If the push URL sits behind a reverse proxy or WAF, note that `kuma-remote` sends push requests with a desktop-Chrome-on-Windows `User-Agent` header (not a generic HTTP-client string) specifically to avoid bot-protection false positives; adjust any allowlists accordingly if you rely on user-agent filtering.
- Unless `auto_update: false` is set, the machine also needs outbound HTTPS access to `api.github.com` and `github.com` (release asset downloads), and the process needs write access to its own install directory so it can replace its executable.

## Building

```sh
cargo build --release
```

The compiled binary is written to `target/release/kuma-remote` (`kuma-remote.exe` on Windows).

## Configuration

Checks are defined in a [StrictYAML](https://hitchdev.com/strictyaml/) file. If `--config` is not given, `kuma-remote` looks in the current directory for `kuma-remote.yaml`, then `kuma-config.yaml`, then `config.yaml`, using the first one that exists. See `kuma-remote.example.yaml` for a working example.

```yaml
debug: false #=-- optional, defaults to false
report_run_failures: true #=-- optional, defaults to true
auto_update: true #=-- optional, defaults to true; checks GitHub for a newer release at startup and self-updates

checks:
  - id: self
    name: "Remote Agent Heartbeat"
    mode: heartbeat #=-- always reports Up with msg "Heartbeat"; host is optional
    host: 8.8.8.8 #=-- optional -- if set, also pings and includes latency, good for pinging 8.8.8.8 — or similar — for internet latency
    push_url: "https://kuma.mydomain.com/api/push/HbEaT456"
    interval: 60s
    
  - id: web01
    name: "Prod Web Server"
    mode: ping
    host: 192.168.1.10
    push_url: "https://kuma.mydomain.com/api/push/AbC123XyZ"
    interval: 60s #=-- It's recommended to set the heartbeat interval to at least 30 seconds longer than the longest expected check interval

  - id: db01
    name: "Database Host"
    mode: ping
    host: db.internal.local
    push_url: "https://kuma.mydomain.com/api/push/QwErTy987"
    interval: 5m
```

Top-level fields:

- `debug` -- optional, defaults to `false`. When `true`, every configured check is logged at startup, and every push logs the exact request URL sent to Kuma, including its query string. Leave this off unless you're actively troubleshooting: a push URL is itself a bearer credential (anyone who has it can push status to your monitor), so `kuma-remote` doesn't log it by default.
- `report_run_failures` -- optional, defaults to `true`. When `true`, a check run that errors out entirely (e.g. an unresolvable hostname) is also pushed to Kuma as a `down` status with the error as `msg`, in addition to being logged. On by default: without it, a run error sends no heartbeat at all, leaving the Kuma monitor stuck on its last known state instead of reflecting the failure. Set to `false` to only log run errors and never push for them.
- `auto_update` -- optional, defaults to `true`. See [Auto-Update](#auto-update) below.
- `checks` -- the list of checks to run, described below.

Fields, per entry under `checks`:

- `id` -- Required. Unique slug for this check. Duplicate ids are a startup error.
- `name` -- Required. Human-readable name, used only in logs.
- `mode` -- Required. Check strategy: `ping` or `heartbeat` (see Check Modes below).
- `host` -- IP address or hostname to check. Required for `ping`. Optional for `heartbeat`: when set, it's also pinged and a successful latency is included in the push; a missing host, or a failed/timed-out ping, doesn't affect the heartbeat's `Up` status.
- `push_url` -- Required. Full Uptime Kuma push URL for this monitor. Kuma's dashboard displays the push URL with a `?status=up&msg=OK&ping=` example suffix attached for you to copy as a curl command; `kuma-remote` builds its own `status`/`msg`/`ping` query string, so if `push_url` still has a `?...` suffix on load, it's stripped automatically and logged as a warning rather than rejected.
- `interval` -- Required. How often to run the check, as a duration string (`30s`, `5m`, `1h`, ...).

The config file must define at least one check; a zero-length `interval`, a duplicate `id`, or a `ping` check missing `host` is rejected at startup.

### Environment variables

- `RUST_LOG` -- Controls log verbosity/filtering (`tracing_subscriber::EnvFilter` syntax). Defaults to `info` for all targets if unset.

## Usage

```sh
kuma-remote [--config <path>]
```

- `-c`, `--config <path>` -- Path to the StrictYAML config file. Default: first of `kuma-remote.yaml`, `kuma-config.yaml`, `config.yaml` (checked in that order) that exists.
- `-h`, `--help` -- Print help.
- `-V`, `--version` -- Print version.

`kuma-remote` has one mode of operation: it loads the config, starts one background task per check, and runs until interrupted (Ctrl-C / SIGINT), at which point all check tasks stop and the process exits. There is no one-shot/run-once flag; scheduling is handled internally, not by an external cron/Task Scheduler. To run continuously across reboots, wrap it in a Windows service or a systemd unit.

## Output

`kuma-remote` writes structured log lines to stdout only (via `tracing_subscriber`); there is no log file. On boot, before loading config, it logs its own name, version, and author(s). Each check logs one `up` or `down` line per run, including the check id, name, and (for ping) latency in milliseconds or the failure reason. There are no report tables — each push result is sent directly to Uptime Kuma, which is the system of record for check history.

## Architecture

```text
config.yaml --> Config::load (StrictYAML) --> scheduler::spawn_all
                                                     |
                                     one tokio task per check, own interval
                                                     |
                                          checks::<mode>::run (e.g. ping)
                                                     |
                                            kuma::push (GET push_url)
```

Each check's task loop is independent: a slow or failing check never blocks or delays any other check's schedule.

## Auto-Update

Unless `auto_update: false` is set, `kuma-remote` checks the latest GitHub release of this repo at startup, before starting any checks. It compares the SHA-256 of its own running executable (not its version number) against the digest GitHub publishes for the matching release asset. If they differ, it downloads the new executable, verifies its hash, replaces itself in place, and exits -- it does **not** relaunch itself.

Restarting into the new version is left entirely to whatever supervises the process, on purpose: if `kuma-remote` also spawned its own replacement, a supervisor that restarts on any exit (NSSM's default `AppExit` behavior, or a systemd unit with `Restart=always`) would restart it too, leaving both processes running permanently and double-reporting every check to Kuma. **To get an update to actually take effect automatically, run `kuma-remote` under a supervisor configured to restart it on exit** (NSSM's default settings already do this; for systemd, use `Restart=always` or `Restart=on-success`). Without such a supervisor -- e.g. running it directly in a console -- an update replaces the file on disk but the process simply stops; it needs a manual restart to pick up the new binary.

Any failure in this process -- no network access, GitHub rate limiting, no matching release asset, no write permission to the install directory, and so on -- is logged and otherwise ignored; it never prevents `kuma-remote` from starting with the currently installed version.

## Modules

- `main.rs` -- CLI parsing, config load, task spawning, shutdown on Ctrl-C.
- `config.rs` -- StrictYAML config schema (`Config`, `CheckConfig`, `CheckMode`) and validation.
- `scheduler.rs` -- Per-check interval loop; dispatches to the check's mode and pushes the result.
- `checks/ping.rs` -- Ping check: single ICMP echo per run, cross-platform via the `pinger` crate.
- `checks/heartbeat.rs` -- Heartbeat check: always reports up, optionally pinging `host` for latency.
- `kuma.rs` -- Uptime Kuma push client (builds the `status`/`msg`/`ping` query string, sends the GET request).
- `logging.rs` -- `tracing` subscriber setup.
- `updater.rs` -- checks GitHub's latest release for a newer build and self-replaces/restarts when found.

## Check Modes

- `ping` -- Sends a single ICMP echo to `host` and reports latency (up) or the failure reason (down). `host` is required.
- `heartbeat` -- Always reports up with message "Heartbeat", signaling the `kuma-remote` process itself is alive rather than testing reachability.
  - `host` is optional; if set, it's also pinged once and a successful latency is included, but a failed or missing ping never turns the heartbeat down.

## License

[![Static Badge](https://img.shields.io/badge/License-PolyForm_Noncommercial_License_1.0.0-582aad)](LICENSE.md)

This project is licensed under the **PolyForm Noncommercial License 1.0.0**.

- **Permitted:** Personal use, hobby projects, research, and non-commercial organization use.
- **Prohibited:** Any commercial application, monetary gain, or use for commercial advantage.

For full terms, please read the [LICENSE](LICENSE.md) file.
