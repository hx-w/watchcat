# Watchcat

[![CI](https://github.com/hx-w/watchcat/actions/workflows/ci.yml/badge.svg)](https://github.com/hx-w/watchcat/actions/workflows/ci.yml)
[![Security audit](https://github.com/hx-w/watchcat/actions/workflows/security.yml/badge.svg)](https://github.com/hx-w/watchcat/actions/workflows/security.yml)
[![Release](https://img.shields.io/github/v/release/hx-w/watchcat?display_name=tag)](https://github.com/hx-w/watchcat/releases/latest)
[![Downloads](https://img.shields.io/github/downloads/hx-w/watchcat/total)](https://github.com/hx-w/watchcat/releases)
[![MSRV](https://img.shields.io/badge/MSRV-1.85-dea584?logo=rust)](https://www.rust-lang.org/)
[![License](https://img.shields.io/github/license/hx-w/watchcat)](LICENSE)

Watchcat resumes interrupted AI coding sessions after failures you choose. It
watches an explicit session list, classifies structured provider errors, applies
a configurable policy, and sends a continuation only after rechecking the
latest turn.

Codex sessions are supported. Claude Code's official failure codes are already
part of the provider-neutral condition model, but Claude session discovery and
resume are not included in this release.

## Install or update

macOS and Linux:

```bash
curl --proto '=https' --tlsv1.2 -LsSf https://raw.githubusercontent.com/hx-w/watchcat/main/scripts/install.sh | sh
```

Windows PowerShell:

```powershell
irm https://raw.githubusercontent.com/hx-w/watchcat/main/scripts/install.ps1 | iex
```

The installers download the latest native GitHub Release, verify its SHA-256
checksum, and replace the existing binary. Run the same command again to update.
They support Linux x86-64 and ARM64, macOS Intel and Apple Silicon, and Windows
x86-64.

Install a specific version or destination when reproducibility matters:

```bash
curl --proto '=https' --tlsv1.2 -LsSf \
  https://raw.githubusercontent.com/hx-w/watchcat/main/scripts/install.sh \
  | WATCHCAT_VERSION=v0.3.0 WATCHCAT_INSTALL_DIR="$HOME/bin" sh
```

```powershell
$env:WATCHCAT_VERSION = "v0.3.0"
$env:WATCHCAT_INSTALL_DIR = "$HOME\bin"
irm https://raw.githubusercontent.com/hx-w/watchcat/main/scripts/install.ps1 | iex
```

Building from source requires Rust 1.85 or newer:

```bash
cargo install --git https://github.com/hx-w/watchcat --locked
```

## Quick start

Watchcat uses the authenticated `codex` command already installed on the same
machine. It needs no separate OpenAI API key.

```bash
watchcat doctor
watchcat session list
watchcat watch add SESSION_ID --label "release task"

# Send an explicit instruction without adding the session to the watchlist.
watchcat session send SESSION_ID "Continue with the release checklist."

# Stop the session's active turn explicitly.
watchcat session interrupt SESSION_ID

# Inspect the effective policy and exercise it without sending a prompt.
watchcat policy list --category capacity
watchcat run --once --dry-run

# Keep watching until interrupted.
watchcat -v run
```

Use `watchcat watch remove SESSION_ID` to revoke permission. A running watchdog
reloads the watchlist before its next reconciliation. It never infers permission
from pinned, recent, or active sessions.

For unattended operation, see the systemd, launchd, and Windows Task Scheduler
examples in [Running in the background](docs/background.md).

## Recovery policies

Conditions are stable names shared by every provider. Network, capacity,
conflict, server, and provider retry-exhaustion conditions retry by default.
Authentication, billing, capability, context, quota, request, sandbox, and
unknown conditions skip by default.

```console
$ watchcat policy list --category capacity
CONDITION                     ACTION  BACKOFF      MAX  CUSTOM
----------------------------  ------  -----------  ---  ------
capacity.model_overloaded     retry   exponential  5    no
capacity.service_overloaded   retry   exponential  5    no
capacity.rate_limited         retry   exponential  5    no
capacity.server_throttled     retry   exponential  5    no
```

Every policy field can be changed from the CLI:

```bash
watchcat policy set capacity.model_overloaded \
  --action retry \
  --backoff exponential \
  --initial-delay 15s \
  --max-delay 5m \
  --max-attempts 8 \
  --prompt "The {model} model is overloaded. Continue the unfinished task. Attempt {attempt}/{max_attempts}."
```

Actions are `retry` and `skip`. Backoff is `fixed` or `exponential`. Durations
accept seconds by default, or the `s`, `m`, and `h` suffixes. Prompt templates
support `{provider}`, `{model}`, `{condition}`, `{provider_code}`, `{attempt}`,
and `{max_attempts}`.

Use `watchcat policy show CONDITION`, `watchcat policy reset CONDITION`, or
`watchcat policy reset --all` to inspect or restore settings. There are no
provider-specific `codes` or `capabilities` commands; condition discovery and
editing belong to `policy`.

## Session logs

`session logs` merges recent provider turns and messages with Watchcat's own
retry lifecycle. Watchcat events include matched conditions, delays, attempts,
the exact retry prompt, and the continuation result.

```bash
watchcat session logs SESSION_ID --limit 30
watchcat session logs SESSION_ID --category capacity
watchcat session logs SESSION_ID --json
```

Provider messages are read on demand. Watchcat stores only its bounded JSONL
event history, which may contain provider failure text and retry prompts.

## Control a session

Send a one-off instruction directly to a session. For sessions owned by Codex
Desktop, Watchcat asks the owning window to steer its active turn or start a new
one through the local Desktop IPC router. This avoids competing for the App
Server's single-writer lock. If Desktop is not running or does not own the
session, Watchcat falls back to a standalone Codex App Server connection.

These are explicit user actions, so the session does not need to be in the
automatic recovery watchlist and recovery policies do not limit them.

```bash
watchcat session send SESSION_ID "Review the current diff and fix the test."
printf '%s\n' 'Read the release checklist.' 'Then continue.' \
  | watchcat session send SESSION_ID
watchcat session send SESSION_ID "Report status" --json
watchcat session interrupt SESSION_ID
watchcat session interrupt SESSION_ID --json
```

`session interrupt` requires an active turn and targets its exact turn ID. An
empty message argument or empty standard input is rejected. `--provider codex`
is the current default; both commands are provider-neutral so future adapters
can expose the same contract.

Codex Desktop IPC is a private, versioned local protocol rather than a public
OpenAI API. Watchcat validates protocol versions and endpoint ownership and
fails closed when compatibility is unknown. Run `watchcat doctor` after a Codex
Desktop update. See [Codex Desktop IPC](docs/codex-desktop-ipc.md) for the
compatibility and fallback contract.

## Commands

| Command | Purpose |
| --- | --- |
| `watchcat status` | Show watched sessions and their latest condition |
| `watchcat run` | Watch continuously |
| `watchcat run --once --dry-run` | Evaluate once without sending anything |
| `watchcat session list` | List recent provider sessions |
| `watchcat session show ID` | Show one provider session |
| `watchcat session logs ID` | Show structured provider and retry history |
| `watchcat session send ID MESSAGE` | Steer an active turn or start a new one |
| `watchcat session interrupt ID` | Stop the exact active turn |
| `watchcat watch list` | List explicitly watched sessions |
| `watchcat watch add ID` | Add one session to the watchlist |
| `watchcat watch remove ID` | Remove one session from the watchlist |
| `watchcat policy list` | List configurable conditions and effective actions |
| `watchcat policy show CONDITION` | Show one effective policy |
| `watchcat policy set CONDITION` | Change retry, backoff, limit, or prompt fields |
| `watchcat policy reset CONDITION` | Restore built-in defaults |
| `watchcat config init` | Write a documented configuration |
| `watchcat config show` | Print the effective configuration |
| `watchcat config path` | Print native configuration and state paths |
| `watchcat config validate` | Validate configuration without connecting |
| `watchcat doctor` | Check configuration and Codex connectivity |

Commands with structured output support `--json`.

## Safety and storage

The engine enforces these invariants:

- Only explicitly watched sessions can change.
- Each failed turn is handled at most once.
- Every retry is delayed and bounded.
- The failed turn is read again immediately before sending a prompt.
- A changed session cancels the pending continuation.
- Automatic recovery only starts a new turn; it never steers active work.
- Unknown failures skip by default.
- Dry-run mode never sends or marks a failure handled.
- One state directory can have only one active runner.

`watchcat config path --json` prints the active paths. The environment variables
`WATCHCAT_CONFIG_DIR`, `WATCHCAT_STATE_DIR`, and `WATCHCAT_WATCHLIST` override
local storage.

Versions 0.2 and 0.3 use configuration, watchlist, and runtime-state schema 2.
Version 1 files are intentionally unsupported. Run
`watchcat config init --force`, review the new configuration, and rebuild the
watchlist when upgrading from 0.1.

Watchcat does not expose a network listener or store provider credentials. Its
Desktop integration connects only to the current user's local IPC endpoint.
Read the [Security policy](SECURITY.md) before running it on a shared machine.

## Development and releases

```bash
cargo test --all-targets --locked
cargo fmt --all --check
cargo clippy --all-targets --locked -- -D warnings
cargo package --locked
```

CI tests Linux, macOS, and Windows, checks Rust 1.85 compatibility, lints the
shell installer, and verifies crate packaging. A matching version tag builds
five native archives, publishes checksums, creates the GitHub Release, and runs
real installer tests on Linux, macOS, and Windows.

Read [Contributing](CONTRIBUTING.md) before proposing a provider or changing the
condition catalog. User-visible changes belong in [Changelog](CHANGELOG.md).

## License

[MIT](LICENSE)
