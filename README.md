# Watchcat

[![CI](https://github.com/hx-w/watchcat/actions/workflows/ci.yml/badge.svg)](https://github.com/hx-w/watchcat/actions/workflows/ci.yml)
[![Security audit](https://github.com/hx-w/watchcat/actions/workflows/security.yml/badge.svg)](https://github.com/hx-w/watchcat/actions/workflows/security.yml)
[![License](https://img.shields.io/github/license/hx-w/watchcat)](LICENSE)

Watchcat is a local reliability manager for AI coding sessions. `watchcatd`
owns provider connections, discovers recent sessions, classifies
structured failures, and applies bounded recovery policies. The `watchcat` CLI
controls the background service.

Codex supports discovery, logs, recovery, send, and interrupt. Claude Code
supports automatic discovery and logs from its native local transcripts. Claude
live-session recovery, send, and interrupt are not supported; Watchcat does not
launch a second Claude process against an existing conversation.

## Architecture

```text
watchcat CLI ─ framed JSON RPC ─ watchcatd ─ Codex App Server
                 local socket       │
                                    ├─ policies and hot reload
                                    ├─ watchlist and lifecycle
                                    └─ runtime state and events
```

The service listens only on a current-user Unix socket. It does not open a TCP
port or store provider credentials. The socket directory is mode `0700`, the
socket is mode `0600`, and macOS/Linux peers must have the daemon user's UID.

## Install or update the CLI and service

macOS and Linux release archives include `watchcat` and `watchcatd`:

```bash
curl --proto '=https' --tlsv1.2 -LsSf \
  https://raw.githubusercontent.com/hx-w/watchcat/main/scripts/install.sh | sh
```

Run the same command again to update. The installer verifies the release
checksum and replaces both `watchcat` and `watchcatd`. Run `watchcat service stop`
before updating and `watchcat service start` after installation. Stop foreground
instances with Ctrl-C. Always upgrade CLI and server together.

Install a specific version or destination when reproducibility matters:

```bash
curl --proto '=https' --tlsv1.2 -LsSf \
  https://raw.githubusercontent.com/hx-w/watchcat/main/scripts/install.sh \
  | WATCHCAT_VERSION=v0.5.0 WATCHCAT_INSTALL_DIR="$HOME/bin" sh
```

Building from source requires Rust 1.85 or newer:

```bash
cargo install --git https://github.com/hx-w/watchcat --locked --bins
```

Supported platforms: macOS and Linux. Windows server transport is not implemented.

## Quick start

Watchcat uses the authenticated `codex` command on the same machine. It needs no
separate OpenAI API key.

```bash
watchcat config init
watchcat service install
watchcat service start
watchcat service status
```

The service starts at login and runs independently of your terminal. See
[Background services](docs/background.md) for logs and custom paths. Use `watchcatd --dry-run` for a foreground check.

Then:

```bash
watchcat session list
watchcat session add SESSION_ID --label "release task"
watchcat service status
```

All runtime commands require `watchcatd` and use local RPC. The CLI never starts
provider processes or runs recovery itself. There is no offline execution mode.

## Background service and recovery control

```bash
watchcat service status
watchcat service stop
watchcat service start
watchcat service restart
watchcat service uninstall
```

Service management uses launchd on macOS and systemd user services on Linux.
Uninstalling the service preserves all configuration, watchlists, and history.
Service lifecycle is the only global on/off control. There is no separate guard switch.

## Watchlist and lifecycle

Every poll (10 seconds by default) discovers Codex and Claude Code sessions,
including existing sessions active within the last three days. No hooks or
manual registration are required. Disable an adapter with
`[providers.codex] enabled = false` or `[providers.claude] enabled = false`.

```bash
watchcat session list
watchcat session list --provider claude
watchcat session search --provider codex
watchcat session add SESSION_ID --provider claude
watchcat session remove SESSION_ID --provider claude
```

`list` shows managed sessions across providers. `search` browses a provider's
catalog, including excluded and old sessions; JSON output includes its next cursor.
Manual removal remains excluded across polls and restarts. A manual add clears
that exclusion. Removal never deletes provider transcripts or stops their work.

```toml
[lifecycle]
stale_after_seconds = 259200
```

Entries expire after three days without provider activity. Polling, recovery
logs, metadata writes, and unresolved failures do not extend that time. A manual
add grants a fresh three-day window. Automatically expired sessions rejoin when
new provider activity appears. Active sessions and sessions whose provider cannot
be checked are retained until their activity can be established.

## Policies

Every known condition is editable. A retry policy owns its action, backoff kind,
initial and maximum delays, attempt limit, and exact recovery prompt.

```bash
watchcat config policy list
watchcat config policy set capacity.model_overloaded \
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
returns the existing operation. RPC callers can follow it to success, failure, or
an explicit unknown result when a provider acknowledgement is lost or the
service restarts; it never claims that an uncertain recovery was not sent.

The recovery counter advances only after the recovery turn is observed as
completed. Starting a retry is not counted as success. The hands-free percentage
is automatic successful recoveries divided by all successful recoveries.

## Safety invariants

- Only current managed sessions can change automatically; exclusions revoke recovery.
- Each failed turn is handled at most once.
- Every retry is delayed and bounded.
- The failed turn is rechecked immediately before a continuation is sent.
- A changed session cancels the pending continuation.
- Automatic recovery starts a new turn and never steers active work.
- Unknown failures skip by default.
- One server owns a state directory.
- Mutations can carry an expected revision and fail on stale client state.

Configuration and watchlist schemas are version 4, control state is version 2,
runtime recovery state stays at version 3, and RPC is version 2. Other versions
are rejected without modification. There is no automatic migration.
To reset an older installation, stop its service, back up the paths printed by
`watchcat config path`, move the old config and state files aside, and run
`watchcat config init` before starting the new server.

## Development

```bash
cargo test --all-targets --locked
cargo fmt --all --check
cargo clippy --all-targets --locked -- -D warnings
```

See [Architecture](docs/architecture.md), [Background services](docs/background.md),
and the [Security policy](SECURITY.md).

## License

[MIT](LICENSE)
