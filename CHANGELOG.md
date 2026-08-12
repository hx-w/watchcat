# Changelog

This project follows [Semantic Versioning](https://semver.org/).

## [Unreleased]

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
