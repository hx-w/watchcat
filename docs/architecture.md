# Architecture

Watchcat 0.4 separates the long-running reliability service from its control
surfaces.

```text
MenuBarExtra / full client            watchcat CLI
              \                         /
               \  protocol v1 JSON RPC /
                +---- Unix socket -----+
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
watchlist lifecycle, configuration revision, and event publication. A UI never
implements recovery itself. The CLI uses the daemon when available and retains
direct mode only for compatibility and diagnostics.

The daemon reconciles provider state periodically. Provider filesystem
notifications reduce latency, but polling remains the correctness fallback.
Configuration file modification time is checked every cycle. A valid change is
atomically activated as a new revision; an invalid change is reported and the
last good revision remains active.

## Local protocol

RPC messages are UTF-8 JSON with a four-byte big-endian length prefix. Requests
are limited to 1 MiB and responses to 8 MiB. Every request carries protocol
version 1 and a request ID. Mutations may include `expected_revision`; a
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
transport is deferred; direct CLI mode remains available there.

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

The watchlist is an authorization list, not session storage. Each target keeps
its addition time, latest observed activity, enabled state, and optional
long-term protection. A stale sweep refreshes provider timestamps before
removing entries. The sweep retains protected targets, unresolved failures, and
all targets for a provider that could not be checked. It never deletes provider
sessions or event history.

## Storage

Configuration, watchlist, and runtime state use schema version 3. Version 2
documents migrate automatically. Files are replaced atomically. The bounded
JSONL event log contains recovery decisions, failure text, and sent recovery
prompts. Full provider messages are fetched on demand.

## Provider contract

Each adapter owns:

1. start and close;
2. session discovery and logs;
3. normalized latest-failure detection;
4. recovery-turn outcome observation;
5. resume, manual send, and interrupt;
6. optional change notification.

Automatic recovery only starts a new turn. Manual send may steer an active
Codex Desktop-owned turn through the local Desktop IPC router. The Desktop
transport validates protocol version and endpoint ownership and fails closed on
unknown compatibility.
