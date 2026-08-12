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
