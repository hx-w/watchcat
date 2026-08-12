# Security policy

## Supported versions

Security fixes are provided for the latest released minor version.

## Reporting a vulnerability

Do not open a public issue for a vulnerability that could resume unauthorized
sessions, disclose conversation data, bypass a provider approval, or execute
commands. Use GitHub's private vulnerability reporting for this repository.

Include the affected version, provider, reproduction, and whether a resume turn
was actually sent. Never attach provider tokens or unredacted session content.

## Local security notes

Watchcat inherits the environment and account permissions of the user running
it. Protect the config, watchlist, and state directories from other users. The
watchlist is an authorization boundary: anyone who can edit it can authorize a
session for automatic continuation.

Watchcat intentionally has no HTTP server, telemetry, or credential store.

## Codex Desktop IPC

Watchcat can connect to Codex Desktop's private local IPC router. It does not
read or store provider tokens, and it does not expose the router over a network.
On Unix, Watchcat rejects the endpoint unless its directory and socket are owned
by the current user and the directory is not writable by group or other users.
On Windows, it verifies that the connected named-pipe server process has the
same user SID as Watchcat before sending any request.

The Desktop protocol is versioned but not a public OpenAI API. Watchcat accepts
only the method versions it implements and reports an error on incompatible
responses. After updating Codex Desktop, run `watchcat doctor` before relying on
unattended recovery.
