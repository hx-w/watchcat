# Watchcat

[![CI](https://github.com/hx-w/watchcat/actions/workflows/ci.yml/badge.svg)](https://github.com/hx-w/watchcat/actions/workflows/ci.yml)
[![Security audit](https://github.com/hx-w/watchcat/actions/workflows/security.yml/badge.svg)](https://github.com/hx-w/watchcat/actions/workflows/security.yml)
[![Release](https://img.shields.io/github/v/release/hx-w/watchcat?display_name=tag)](https://github.com/hx-w/watchcat/releases/latest)
[![Downloads](https://img.shields.io/github/downloads/hx-w/watchcat/total)](https://github.com/hx-w/watchcat/releases)
[![MSRV](https://img.shields.io/badge/MSRV-1.85-dea584?logo=rust)](https://www.rust-lang.org/)
[![License](https://img.shields.io/github/license/hx-w/watchcat)](LICENSE)

Watchcat keeps long-running AI coding sessions moving when a transient network
failure ends a turn. It watches an explicit allowlist, inspects structured turn
state, and starts a continuation only when the latest failure is safe to retry.

Codex is supported today. The recovery engine, durable state, CLI, and provider
contract are intentionally provider-neutral, so Claude and other agents can be
added without widening Codex recovery rules.

Watchcat never retries approvals, user-input requests, authentication failures,
usage limits, context limits, sandbox failures, or normal completed turns.

## Install or update

### macOS and Linux

```bash
curl --proto '=https' --tlsv1.2 -LsSf https://raw.githubusercontent.com/hx-w/watchcat/main/scripts/install.sh | sh
```

The installer selects the native GitHub Release, verifies its SHA-256 checksum,
and places `watchcat` in `~/.local/bin`. Run the same command again to update.

### Windows PowerShell

```powershell
irm https://raw.githubusercontent.com/hx-w/watchcat/main/scripts/install.ps1 | iex
```

The Windows installer verifies the release checksum, installs under
`%LOCALAPPDATA%\Programs\watchcat\bin`, and adds that directory to the user
`PATH`. Run the same command again to update.

Install a specific release or location when reproducibility matters:

```bash
curl --proto '=https' --tlsv1.2 -LsSf \
  https://raw.githubusercontent.com/hx-w/watchcat/main/scripts/install.sh \
  | WATCHCAT_VERSION=v0.1.0 WATCHCAT_INSTALL_DIR="$HOME/bin" sh
```

```powershell
$env:WATCHCAT_VERSION = "v0.1.0"
$env:WATCHCAT_INSTALL_DIR = "$HOME\bin"
irm https://raw.githubusercontent.com/hx-w/watchcat/main/scripts/install.ps1 | iex
```

Supported release targets are Linux x86-64 and ARM64, macOS Intel and Apple
Silicon, and Windows x86-64. Building from source requires Rust 1.85 or newer:

```bash
cargo install --git https://github.com/hx-w/watchcat --locked
```

## Quick start

Watchcat requires an authenticated `codex` command on the same machine as the
sessions it watches.

```bash
# Check Codex App Server connectivity and local paths.
watchcat doctor

# Find the session ID and explicitly authorize one session.
watchcat list
watchcat add 019ff1ab-fee9-7fd2-99ca-255cca3d55e0 --label "release task"

# Inspect it, then exercise the decision path without sending a message.
watchcat status
watchcat run --once --dry-run

# Keep watching until interrupted.
watchcat -v run
```

Use `watchcat remove SESSION_ID` to revoke permission. A running watchdog reloads
the watchlist before its next reconciliation. Watchcat does not infer
authorization from pinned, recent, or active sessions.

For unattended operation, use the examples for systemd, launchd, and Windows
Task Scheduler in [Running in the background](docs/background.md).

## Safety model

Watchcat resumes only these Codex failure classes:

- `HttpConnectionFailed` when the status is transient
- `ResponseStreamConnectionFailed`
- `ResponseStreamDisconnected`
- `ResponseTooManyFailedAttempts`

Older Codex versions may omit the structured code. Watchcat recognizes only a
narrow set of equivalent disconnect messages and treats every unknown failure
as non-retryable.

The engine also enforces these invariants:

- Only sessions in the explicit watchlist can be changed.
- The latest turn is read again immediately before sending a continuation.
- Each failed turn is handled at most once.
- Recovery uses configurable backoff and a per-session hourly limit.
- One state directory can have only one active runner.
- Dry-run mode never sends a prompt or marks the failure handled.

The default backoff is 5, 30, and 120 seconds, with at most three continuation
attempts per session in one hour.

## How it works

Watchcat starts `codex app-server --listen stdio://` and speaks JSON-RPC over
standard input and output. It reads existing desktop, IDE, and CLI session logs
through the App Server. It does not use keyboard automation, expose a network
listener, or need a separate OpenAI API key.

```text
watchcat CLI
    |
    +-- WatchEngine: allowlist, backoff, rate limit, dedupe, race check
    |
    +-- Provider trait
            |
            +-- CodexProvider -- JSON-RPC stdio -- codex app-server
            |
            +-- future providers
```

Filesystem notifications reduce latency. Periodic reconciliation remains the
correctness fallback when notifications are unavailable or lost. See
[Architecture](docs/architecture.md) for provider and compatibility contracts.

## Commands

| Command | Purpose |
| --- | --- |
| `watchcat init` | Write a documented default configuration |
| `watchcat list` | List provider sessions and watch status |
| `watchcat add ID` | Add one session to the explicit watchlist |
| `watchcat remove ID` | Remove one session from the watchlist |
| `watchcat status` | Show watched sessions and the latest failure |
| `watchcat run` | Watch continuously |
| `watchcat run --once --dry-run` | Evaluate once without sending anything |
| `watchcat doctor` | Check configuration and provider connectivity |
| `watchcat paths` | Print native configuration and state paths |

Discovery and status commands support `--json` for scripting.

## Configuration and privacy

Run `watchcat init` to create a commented configuration, or copy
[`config.example.toml`](config.example.toml). Native paths differ by platform;
`watchcat paths --json` prints the exact locations in use.

The following environment variables override local storage:

- `WATCHCAT_CONFIG_DIR`
- `WATCHCAT_STATE_DIR`
- `WATCHCAT_WATCHLIST`

Watchcat stores session IDs, optional labels, timestamps, deduplication keys,
and resume-attempt history. It does not copy conversation content or provider
credentials into its own state. Read [Security policy](SECURITY.md) before
running it on a shared machine.

## Development and releases

```bash
cargo test --all-targets --locked
cargo fmt --all --check
cargo clippy --all-targets --locked -- -D warnings
cargo package --locked
```

CI runs the test suite on Linux, macOS, and Windows, checks Rust 1.85
compatibility, lints the shell installer, and verifies crate packaging. A
matching `vMAJOR.MINOR.PATCH` tag builds five native archives, publishes
SHA-256 checksums, creates the GitHub Release, and performs real install tests
on Linux and Windows.

Read [Contributing](CONTRIBUTING.md) before proposing a provider or changing the
recovery allowlist. User-visible changes belong in [Changelog](CHANGELOG.md).

## License

[MIT](LICENSE)
