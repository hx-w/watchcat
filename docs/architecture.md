# Architecture

Watchcat separates product integration from recovery policy. A provider may
change its transport or schema without changing which failures Watchcat is
allowed to recover.

```text
CLI / service runner
        |
        v
WatchEngine ---- WatchlistStore
    |   |        RuntimeStateStore
    |   +---- backoff / rate limit / dedupe / race check
    |
Provider contract
    |
    +---- CodexProvider ---- JSON-RPC stdio ---- codex app-server
    |
    +---- future ClaudeProvider
```

## Provider contract

Each provider implements:

1. `start()` and `close()` for resource ownership.
2. `list_sessions()` for read-only discovery.
3. `latest_failure()` to translate the provider's newest turn into a generic
   `Failure` or return `None`.
4. `resume()` to create one continuation turn.
5. `wait_for_change()` as an optional wakeup optimization. The default is
   polling, so change notifications are never required for correctness.

Provider errors are data translation, transport, or authentication concerns.
Retry eligibility is emitted as part of `Failure`, but the provider must be
conservative: ambiguity means `retryable = false`.

## Recovery invariants

The engine enforces these invariants for every provider:

- A session is mutable only when it appears in the explicit watchlist.
- One failure turn is handled at most once.
- A retry is delayed and bounded per session.
- The latest failure is read again immediately before `resume()`.
- A newer completed, active, or failed turn cancels the pending action.
- Dry-run mode never records a failure as handled.
- Only one watchdog process may own a state directory.

## Codex adapter

The adapter launches `codex app-server --listen stdio://`, initializes the
JSON-RPC connection, and reads all interactive source kinds. Desktop tasks are
persisted as Codex threads and are visible to an independent App Server process.

For low latency the adapter registers `fs/watch` on watched rollout files. A
periodic reconciliation still runs after every timeout, covering lost file
events, unsupported watch APIs, and restarts. `thread/turns/list` is used for a
bounded latest-turn read; older App Servers fall back to `thread/read`.

Resume is a two-step operation: `thread/resume`, then `turn/start`. The engine's
second latest-turn check happens before those calls.

## Compatibility policy

- State and watchlist files carry integer schema versions.
- Unknown configuration fields fail closed. New optional fields must have safe
  defaults so older state remains readable by newer releases.
- Unsupported higher schema versions fail closed instead of being rewritten.
- Provider names are part of durable keys (`provider:session:turn`).
- Existing provider semantics cannot be silently broadened by adding another
  provider.

## Threat model

Watchcat assumes the local account already has authority to use the watched
agent sessions. A malicious local user who can modify the watchlist or state
directory already has equivalent account access. Watchcat does not expose a
network listener and does not store provider credentials.
