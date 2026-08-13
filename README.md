# Watchcat

[![CI](https://github.com/hx-w/watchcat/actions/workflows/ci.yml/badge.svg)](https://github.com/hx-w/watchcat/actions/workflows/ci.yml)
[![Security audit](https://github.com/hx-w/watchcat/actions/workflows/security.yml/badge.svg)](https://github.com/hx-w/watchcat/actions/workflows/security.yml)
[![License](https://img.shields.io/github/license/hx-w/watchcat)](LICENSE)

Watchcat is a local reliability manager for AI coding sessions. `watchcatd`
owns provider connections, watches an explicit session list, classifies
structured failures, and applies bounded recovery policies. The CLI and native
macOS client are control surfaces for the same daemon.

Codex sessions are supported. Claude Code failure codes are part of the shared
condition model, but a Claude session adapter is not included yet.

## Architecture

```text
macOS menu bar app ─┐
                    ├─ framed JSON RPC ─ watchcatd ─ Codex App Server
watchcat CLI ───────┘     local socket      │
                                             ├─ policies and hot reload
                                             ├─ watchlist and lifecycle
                                             └─ runtime state and events
```

The service listens only on a current-user Unix socket. It does not open a TCP
port or store provider credentials. The socket directory is mode `0700`, the
socket is mode `0600`, and macOS/Linux peers must have the daemon user's UID.

## Build the macOS client

The native SwiftUI client requires macOS 13 or newer. It provides a compact menu
bar surface and a full window for watchlist, policy, activity, and connection
management.

```bash
./scripts/build-macos-app.sh
open dist/Watchcat.app
```

The app bundle contains the service, CLI, and a bundled LaunchAgent. Enabling
launch at login also synchronizes the matching CLI and service to
`~/.local/bin`. Make sure that directory precedes any older Watchcat install in
your shell's `PATH`.

The GitHub Release includes Intel and Apple Silicon preview app archives. They
are ad-hoc signed and require a one-time macOS Privacy & Security approval; see
the README inside each archive. Trusted distribution without that manual step
still requires Developer ID signing and notarization.

## Install or update the CLI and service

macOS and Linux release archives include `watchcat` and `watchcatd`:

```bash
curl --proto '=https' --tlsv1.2 -LsSf \
  https://raw.githubusercontent.com/hx-w/watchcat/main/scripts/install.sh | sh
```

Run the same command again to update. The installer verifies the release
checksum and replaces both `watchcat` and `watchcatd`. Stop a manually launched
service before updating, then restart it after installation. The macOS app uses
its bundled service and updates it with the app. Upgrade CLI and service
together: version 3 configuration written by Watchcat 0.4 is not readable by
0.3.x.

Install a specific version or destination when reproducibility matters:

```bash
curl --proto '=https' --tlsv1.2 -LsSf \
  https://raw.githubusercontent.com/hx-w/watchcat/main/scripts/install.sh \
  | WATCHCAT_VERSION=v0.4.0 WATCHCAT_INSTALL_DIR="$HOME/bin" sh
```

Building from source requires Rust 1.85 or newer:

```bash
cargo install --git https://github.com/hx-w/watchcat --locked --bins
```

Windows users can rerun `scripts/install.ps1` to update the CLI. Windows
continues to support direct CLI mode; the service's named-pipe
transport is not included in 0.4.0.

## Quick start

Watchcat uses the authenticated `codex` command on the same machine. It needs no
separate OpenAI API key.

```bash
watchcat config init
watchcat doctor
watchcatd
```

In another terminal:

```bash
watchcat session list
watchcat watch add SESSION_ID --label "release task"
watchcat status
```

When `watchcatd` is online, status, session, watchlist, and policy commands use
RPC. Without the daemon, the CLI retains its direct compatibility mode.

## Watchlist and lifecycle

Only explicitly watched sessions may be recovered automatically:

```bash
watchcat watch list
watchcat watch add SESSION_ID
watchcat watch remove SESSION_ID
```

Version 3 configuration includes lifecycle cleanup:

```toml
[lifecycle]
stale_after_seconds = 259200
sweep_interval_seconds = 60
protect_unresolved_failures = true
```

The default removes a watch entry after three days without provider or Watchcat
activity. Protected targets, unresolved failures, and targets whose provider
could not be checked are retained. Cleanup never deletes the provider session.

## Policies

Every known condition is editable. A retry policy owns its action, backoff kind,
initial and maximum delays, attempt limit, and exact recovery prompt.

```bash
watchcat policy list
watchcat policy set capacity.model_overloaded \
  --action retry \
  --backoff exponential \
  --initial-delay 15s \
  --max-delay 5m \
  --max-attempts 8 \
  --prompt "Continue the unfinished task. Attempt {attempt}/{max_attempts}."
```

Prompt templates support `{provider}`, `{model}`, `{condition}`,
`{provider_code}`, `{attempt}`, and `{max_attempts}`. Unknown conditions skip by
default. Daemon-side changes are validated, written atomically, and published as
a new revision. Valid external edits are hot-reloaded; invalid edits leave the
last good settings active.

## Session actions and activity

```bash
watchcat session send SESSION_ID "Continue with the release checklist."
watchcat session interrupt SESSION_ID
watchcat session logs SESSION_ID --limit 30
watchcat session logs SESSION_ID --category capacity
```

Activity is always scoped to a named session. Watchcat stores a bounded JSONL
history of failure and recovery events, not a copy of the full conversation.
Provider messages are read on demand.

Manual retry is accepted as a durable background operation and returns an
operation ID before provider work begins. Repeating an unacknowledged request
returns the existing operation. The client follows it to success, failure, or
an explicit unknown result when a provider acknowledgement is lost or the
service restarts; it never claims that an uncertain recovery was not sent.

The recovery counter advances only after the recovery turn is observed as
completed. Starting a retry is not counted as success. The hands-free percentage
is automatic successful recoveries divided by all successful recoveries.

## Safety invariants

- Only explicitly watched sessions can change automatically.
- Each failed turn is handled at most once.
- Every retry is delayed and bounded.
- The failed turn is rechecked immediately before a continuation is sent.
- A changed session cancels the pending continuation.
- Automatic recovery starts a new turn and never steers active work.
- Unknown failures skip by default.
- One daemon or direct runner owns a state directory.
- Mutations can carry an expected revision and fail on stale client state.

Configuration, watchlist, and runtime-state schemas are version 3. Version 2
files migrate automatically and are rewritten atomically. Version 1 remains
unsupported.

## Development

```bash
cargo test --all-targets --locked
cargo fmt --all --check
cargo clippy --all-targets --locked -- -D warnings
swift test --package-path clients/macos
```

See [Architecture](docs/architecture.md), [Background services](docs/background.md),
and the [Security policy](SECURITY.md).

## License

[MIT](LICENSE)
