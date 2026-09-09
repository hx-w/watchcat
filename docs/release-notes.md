Watchcat 0.5.0 runs as a CLI and a background service on macOS and Linux. The
native macOS app is removed. The CLI now has three command groups: `service`,
`session`, and `config`; recovery policies live under `config policy`, and
service start/stop replaces the separate guard switch.

The service automatically discovers recent Codex and Claude Code sessions and
removes entries after three days without activity. Manual add and remove remain
available: removed sessions stay excluded until added again, while automatically
expired sessions rejoin when new activity appears. Codex supports automatic
recovery, send, and interrupt. Claude Code supports discovery and logs only.

This release intentionally breaks compatibility with older commands, RPC, and
configuration. Stop the old service, back up the paths printed by
`watchcat config path`, and reset configuration and membership state before
starting 0.5.0; see the README for schema versions and setup. Upgrade `watchcat`
and `watchcatd` together. Provider conversations are not deleted by cleanup.

**Full Changelog**: https://github.com/hx-w/watchcat/compare/v0.4.0...v0.5.0
