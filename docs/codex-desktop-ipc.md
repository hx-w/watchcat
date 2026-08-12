# Codex Desktop IPC

Codex Desktop keeps one writer for an active session. Starting a separate
`codex app-server` and calling `thread/resume` can therefore fail with
`already has an active writer`. Watchcat 0.3 can instead route a request to the
Desktop client that already owns the session.

## Transport contract

On macOS and Linux, Watchcat connects to `$CODEX_HOME/ipc/ipc.sock`, or
`~/.codex/ipc/ipc.sock` when `CODEX_HOME` is unset. On Windows it connects to
`\\.\pipe\codex-ipc`. Messages are length-prefixed JSON frames: a four-byte
little-endian payload length followed by UTF-8 JSON.

Before using a Unix endpoint, Watchcat verifies that the socket and its parent
directory belong to the current user and that the directory is not writable by
other users. On Windows, it compares the named-pipe server process SID with the
current process SID.

The client initializes with the router, discovers the owner of the requested
local conversation, then uses one of three fixed follower operations:

- start a turn;
- steer an active turn;
- interrupt an expected active turn.

Watchcat does not offer an arbitrary IPC command or accept an arbitrary router
address. It follows the standard `CODEX_HOME` location and does not access
authentication material.

## Delivery rules

`watchcat session send` first attempts to steer. If the owner reports that the
session is idle, it starts a turn. If a turn becomes active in that small race
window, Watchcat makes one final steer attempt. This bounds delivery to one
logical message without an unbounded race loop.

The unattended recovery engine is stricter: it only starts a new turn. If one
is already active, recovery reports an error and leaves that work unchanged.
`watchcat session interrupt` is always an explicit user action and names the
turn it expects to stop.

If the Desktop router is absent or no Desktop client owns the session, Watchcat
uses its existing standalone App Server transport. A present but insecure,
malformed, timed-out, or version-incompatible router is an error; Watchcat does
not hide it by falling back.

## Compatibility

This is a private Codex Desktop protocol, not a documented OpenAI integration
API. Watchcat pins every method version it uses, limits frames to 256 MiB, and
fails closed on unknown responses. A future Desktop release may require a
Watchcat update.

Run the following after updating either program:

```bash
watchcat doctor
watchcat session send SESSION_ID "Report current status" --json
```

The JSON receipt identifies `desktop_ipc` or `app_server` as the transport. Use
a non-critical session for the delivery check.
