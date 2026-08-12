# Architecture

Watchcat separates provider integration, failure classification, recovery
policy, and observability. Provider-specific error names never control retry
behavior directly.

```text
CLI / service runner
        |
        v
WatchEngine -------- WatchlistStore
    |   |             RuntimeState
    |   +-----------> EventLogStore (JSONL)
    |   +-----------> condition policy / backoff / prompt rendering
    |
Provider contract
    |
    +---- CodexProvider ---- JSON-RPC stdio ---- codex app-server
    |          |
    |          +---- framed local IPC ---- Codex Desktop owner
    |
    +---- future session adapters

Provider errors ---- classifier ---- condition ---- policy ---- action
```

## Conditions and policies

A provider adapter translates a structured upstream failure into a stable
condition such as `network.stream_failed`, `capacity.model_overloaded`, or
`capability.model_unavailable`. The engine resolves that condition through the
user's policy. Providers do not decide whether to send a continuation.

The built-in defaults retry transient network, capacity, conflict, and server
conditions. Authentication, billing, capability, context, quota, request,
sandbox, and unknown conditions are skipped. Unknown input always fails closed.

Each retry policy owns its action, backoff kind, initial and maximum delay,
attempt limit, and prompt template. `retry_after_seconds` from a provider is a
minimum delay and cannot shorten the configured backoff.

## Provider contract

Each session adapter implements:

1. `start()` and `close()` for resource ownership.
2. `list_sessions()` for discovery.
3. `session_logs()` for provider-native turns and messages.
4. `latest_failure()` for normalized failure detection.
5. `resume()` for one continuation turn.
6. `send_message()` for explicit start-or-steer delivery.
7. `interrupt()` for explicit active-turn cancellation when supported.
8. `wait_for_change()` as an optional wakeup optimization.

Claude Code's official `StopFailure` codes are normalized and tested in the
shared classifier. This release does not include a Claude session adapter or
claim that it can resume Claude sessions.

## Recovery invariants

- A session is mutable only when it appears in the explicit watchlist.
- One failed turn is handled at most once.
- Retry attempts are delayed and bounded per session and time window.
- The latest failed turn is read again immediately before `resume()`.
- A changed session cancels the pending continuation.
- Automatic recovery never steers an active turn.
- Dry-run mode never sends a prompt or marks a failure handled.
- One watchdog process may own a state directory.
- Provider and Watchcat events are retained as bounded JSONL history.

## Codex adapter

The adapter starts `codex app-server --listen stdio://`, initializes the
JSON-RPC connection, and reads interactive Codex threads. `thread/read` supplies
structured turns and message items for `session logs`.

When Codex Desktop owns a thread, a second transport connects to its local IPC
router and targets the owning Desktop client. Manual messages steer an active
turn or start an idle turn. Automatic recovery only attempts a new turn; it
never changes the instructions of already-running work. A missing Desktop owner
falls back to `thread/resume` plus App Server `turn/start`; an incompatible or
untrusted Desktop endpoint fails closed.

Filesystem notifications reduce latency. Periodic reconciliation remains the
correctness fallback for lost events, unsupported watch APIs, and restarts.

## Storage contract

Configuration, watchlist, and runtime-state documents use schema version 2.
Watchcat 0.2 intentionally does not migrate version 1 files. Replace an old
configuration with `watchcat config init --force`, then rebuild the watchlist.

The event log is append-only JSONL with bounded compaction. It may contain
failure messages and the exact continuation prompts Watchcat sent, but it does
not copy full provider conversations. Provider messages are read on demand.

## Threat model

Watchcat assumes the local account already has authority to use watched agent
sessions. It exposes no network listener and stores no provider credentials.
The Codex Desktop transport validates the ownership and permissions of Unix IPC
endpoints before connecting and uses only a fixed endpoint and fixed methods.
Anyone who can edit the configuration, watchlist, or state directory should be
treated as having equivalent local account access.
