# Architecture

Watchcat consists of a CLI and a long-running background reliability service.

```text
                   watchcat CLI
                         |
                  protocol v2 JSON RPC
                         |
                     Unix socket
                         |
                     watchcatd
          +----------------+----------------+
          |                |                |
     WatchEngine      durable stores   event broadcast
          |
    provider contract
          |
    Codex App Server ---- Codex Desktop IPC owner
```

## Ownership

`watchcatd` is the only component that owns provider processes, recovery state,
watchlist lifecycle, configuration revision, and event publication during service
operation. All CLI runtime commands use the daemon; an unavailable server
produces an error without starting providers or modifying runtime files. `watchcat service` registers and controls
`watchcatd` through launchd or systemd without an app bundle. The Rust
`client` module is the CLI's RPC transport, not a graphical client.

The daemon reconciles provider state periodically. Configuration file modification time is checked every cycle. A valid change is
atomically activated as a new revision; an invalid change is reported and the
last good revision remains active.

## Local protocol

RPC messages are UTF-8 JSON with a four-byte big-endian length prefix. Requests
are limited to 1 MiB and responses to 8 MiB. Every request carries protocol
version 2 and a request ID. Mutations may include `expected_revision`; a
mismatch fails without applying the change. Session discovery uses an opaque
provider cursor so activity-driven reordering cannot corrupt pagination.
`events.subscribe` keeps one connection open for state and engine notifications
and receives heartbeats during quiet periods.

Manual retry is a short, idempotent accepted-operation request. The operation
is persisted before acknowledgement, and a repeated client request recovers
the same operation ID. The client then queries `retry_status` until the result
is succeeded, failed, or explicitly unknown. Provider acknowledgement loss and
daemon restart therefore never masquerade as an unsent retry.

Provider calls run outside the control-plane lock. Local RPC connections and
event subscriptions have separate bounded capacities, request frames have a
read deadline, and sparse session searches scan a bounded number of provider
pages per request. Service shutdown first revokes the recovery permit and then
cancels queued accepted operations, so no new continuation starts after the
stop signal.

The macOS/Linux endpoint is a Unix domain socket in the native Watchcat state
directory. The directory is mode `0700`, the socket is mode `0600`, and the
accepted peer UID must match the daemon's effective UID. Windows named-pipe
transport is not implemented, so Windows is not a supported runtime platform.

## Conditions and recovery

Providers translate structured errors into stable conditions such as
`network.stream_failed`, `capacity.model_overloaded`, and
`capability.model_unavailable`. Policy resolution, backoff, prompts, and attempt
limits belong to the engine rather than the provider.

A sent recovery becomes pending. It is counted as successful only when the
provider reports the new turn as completed, and counted as failed only when that
turn reports failure. In-progress or unknown state changes no metric. Pending
recovery outcomes continue to be observed after their session leaves the
watchlist; removal revokes future sends, not audit completion.
An outcome that remains unavailable is eventually recorded as unconfirmed and
removed from pending state without being counted as success or failure.

## Session lifecycle

The managed list is an authorization list, not session storage. Each poll scans
enabled providers, paging newest-first until the inactivity cutoff. Discovery
works with an empty list. Each target stores its addition time, provider activity,
and label. Manual removals persist as exclusions in the same atomic document;
manual additions clear them. Automatic expiry leaves no exclusion, so new
activity can bring a session back.

Provider activity, not Watchcat events, determines expiry. Active/unknown sessions
and provider lookup failures are protected from uncertain cleanup. A manual add
starts a fresh inactivity window. Unresolved failures do not prevent expiry.
Scans run outside the control lock and commit only at their original revision;
a concurrent manual removal invalidates the scan. Recovery sends require the
current membership permit. Service shutdown revokes that permit; no separate
guard or per-session pause state exists.

Claude reads top-level native project transcripts, skips subagents, incrementally
parses complete appended records, and derives activity only from user/assistant
timestamps. It supports discovery and logs, not live session mutation. Codex
continues to use its native app-server and Desktop IPC protocols.

## Storage

Configuration and watchlist require schema 4, control state requires schema 2,
and runtime recovery state requires schema 3. Other
versions are rejected without migration. Files are replaced atomically. The
bounded JSONL event log contains recovery decisions, failure text, and sent recovery
prompts. Full provider messages are fetched on demand.

## Provider contract

Each adapter owns:

1. start and close;
2. session discovery and logs;
3. normalized latest-failure detection;
4. recovery-turn outcome observation;
5. resume, manual send, and interrupt.

Automatic recovery only starts a new turn. Manual send may steer an active
Codex Desktop-owned turn through the local Desktop IPC router. The Desktop
transport validates protocol version and endpoint ownership and fails closed on
unknown compatibility.

## Read-only provider smoke check

Run `watchcatd --dry-run` with isolated `WATCHCAT_CONFIG_DIR`,
`WATCHCAT_STATE_DIR`, and `WATCHCAT_WATCHLIST` paths, then use the same
overrides with `watchcat session list --json` and
`watchcat session logs SESSION_ID --provider claude --limit 1`. This exercises
real local discovery and transcript reads without sending provider messages.
Stop that daemon with Ctrl-C after checking. Claude respects `CLAUDE_CONFIG_DIR`;
its [documented session storage](https://code.claude.com/docs/en/agent-sdk/sessions)
is `projects/<project>/*.jsonl` under that directory (default `~/.claude`).
