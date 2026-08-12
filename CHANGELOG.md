# Changelog

This project follows [Semantic Versioning](https://semver.org/).

## [Unreleased]

## [0.3.1] - 2026-08-12

### Fixed

- Treat a Codex Desktop turn that ends during `session send` as an idle-session
  race and start a new turn instead of returning an error.
- Reuse one client message ID across the bounded steer/start retry sequence so
  Codex can correlate every attempt with the same logical message.

## [0.3.0] - 2026-08-12

### Added

- Codex Desktop IPC transport for messaging sessions already owned by the
  Desktop app without acquiring a competing App Server writer.
- `watchcat session interrupt` for explicitly stopping the exact active turn.
- Desktop IPC diagnostics and transport fields in structured receipts.

### Changed

- Manual `session send` now steers through the Desktop owner when available and
  starts a new turn when idle, with one bounded race retry.
- Automatic recovery may start a new turn through Desktop but never steers an
  active turn.

### Security

- Unix Desktop IPC endpoints are accepted only when their directory and socket
  are owned by the current user and the directory is not group/world writable.
- Private protocol version mismatches fail closed instead of silently falling
  back.

## [0.2.1] - 2026-08-12

### Added

- `watchcat session send` for steering an active turn or starting a new turn
  with a message from an argument or standard input and structured receipt
  output.

## [0.2.0] - 2026-08-12

### Added

- Configurable retry or skip policies for network, capacity, capability,
  context, quota, request, authentication, and provider failures.
- Fixed and exponential backoff, per-condition attempt limits, provider retry
  delays, and prompt templates with runtime variables.
- Structured session logs that merge Codex turns and messages with Watchcat's
  retry decisions and sent prompts.
- Official Claude Code `StopFailure` classification as the foundation for a
  future Claude session adapter.

### Changed

- Replaced flat CLI commands with `session`, `watch`, `policy`, and `config`
  command groups.
- Moved retry eligibility out of providers and into the user-editable condition
  policy layer.
- Upgraded configuration, watchlist, and runtime-state schemas to version 2.
  Version 1 files are not migrated.

## [0.1.0] - 2026-08-12

### Added

- Provider-neutral recovery engine and durable watchlist.
- Codex App Server provider with structured network-failure classification.
- Backoff, per-session rate limits, turn-level deduplication, and race checks.
- Filesystem change wakeups with polling fallback.
- Discovery, status, dry-run, diagnostics, and JSON CLI output.
- Checksum-verified installers and native release builds for Linux, macOS, and
  Windows.
