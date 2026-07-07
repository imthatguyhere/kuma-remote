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

## Building

```sh
cargo build --release
```

The compiled binary is written to `target/release/kuma-remote` (`kuma-remote.exe` on Windows).

## Configuration

Checks are defined in a [StrictYAML](https://hitchdev.com/strictyaml/) file. By default `kuma-remote` looks for `kuma-remote.yaml` in the current directory; use `--config` to point elsewhere. See `kuma-remote.example.yaml` for a working example.

```yaml
debug: false # optional, defaults to false
report_run_failures: true # optional, defaults to true

checks:
  - id: web01
    name: "Prod Web Server"
    mode: ping
    host: 192.168.1.10
    push_url: "https://kuma.mydomain.com/api/push/AbC123XyZ"
    interval: 60s

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
- `checks` -- the list of checks to run, described below.

Fields, per entry under `checks` (all required):

- `id` -- Unique slug for this check. Duplicate ids are a startup error.
- `name` -- Human-readable name, used only in logs.
- `mode` -- Check strategy. Currently only `ping` (see Tracked Software below).
- `host` -- IP address or hostname to check.
- `push_url` -- Full Uptime Kuma push URL for this monitor. Kuma's dashboard displays the push URL with a `?status=up&msg=OK&ping=` example suffix attached for you to copy as a curl command; `kuma-remote` builds its own `status`/`msg`/`ping` query string, so if `push_url` still has a `?...` suffix on load, it's stripped automatically and logged as a warning rather than rejected.
- `interval` -- How often to run the check, as a duration string (`30s`, `5m`, `1h`, ...).

The config file must define at least one check; a zero-length `interval` or a duplicate `id` is rejected at startup.

### Environment variables

- `RUST_LOG` -- Controls log verbosity/filtering (`tracing_subscriber::EnvFilter` syntax). Defaults to `info` for all targets if unset.

## Usage

```sh
kuma-remote [--config <path>]
```

- `-c`, `--config <path>` -- Path to the StrictYAML config file. Default: `kuma-remote.yaml`.
- `-h`, `--help` -- Print help.
- `-V`, `--version` -- Print version.

`kuma-remote` has one mode of operation: it loads the config, starts one background task per check, and runs until interrupted (Ctrl-C / SIGINT), at which point all check tasks stop and the process exits. There is no one-shot/run-once flag; scheduling is handled internally, not by an external cron/Task Scheduler. To run continuously across reboots, wrap it in a Windows service or a systemd unit.

## Output

`kuma-remote` writes structured log lines to stdout only (via `tracing_subscriber`); there is no log file. Each check logs one `up` or `down` line per run, including the check id, name, and (for ping) latency in milliseconds or the failure reason. There are no report tables — each push result is sent directly to Uptime Kuma, which is the system of record for check history.

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

## Modules

- `main.rs` -- CLI parsing, config load, task spawning, shutdown on Ctrl-C.
- `config.rs` -- StrictYAML config schema (`Config`, `CheckConfig`, `CheckMode`) and validation.
- `scheduler.rs` -- Per-check interval loop; dispatches to the check's mode and pushes the result.
- `checks/ping.rs` -- Ping check: single ICMP echo per run, cross-platform via the `pinger` crate.
- `kuma.rs` -- Uptime Kuma push client (builds the `status`/`msg`/`ping` query string, sends the GET request).
- `logging.rs` -- `tracing` subscriber setup.

## Check Modes

- `ping` -- Sends a single ICMP echo to `host` and reports latency (up) or the failure reason (down).

## License

[![Static Badge](https://img.shields.io/badge/License-PolyForm_Noncommercial_License_1.0.0-582aad)](LICENSE.md)

This project is licensed under the **PolyForm Noncommercial License 1.0.0**.

- **Permitted:** Personal use, hobby projects, research, and non-commercial organization use.
- **Prohibited:** Any commercial application, monetary gain, or use for commercial advantage.

For full terms, please read the [LICENSE](LICENSE.md) file.
