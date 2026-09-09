# Running watchcatd in the background

Install `watchcat` and `watchcatd` in the same stable directory (the release
installer defaults to `~/.local/bin`). No desktop app is needed.

```bash
watchcat config init
watchcat config validate
watchcatd --dry-run
# Stop the foreground check with Ctrl-C, then:
watchcat service install --dry-run
watchcat service install
watchcat service start
watchcat service status
```

`install` registers start-at-login without starting the server now. `start`
launches it under the OS supervisor, independently of the terminal. `status`
checks the local RPC connection and reports managed sessions and recovery state.
The server automatically discovers recent sessions as soon as it starts.

```bash
watchcat service stop
watchcat service start
watchcat service restart
watchcat service uninstall
```

Stop the service before updating both binaries, then start it again.
`uninstall` stops the server and removes the service registration, preserving
configuration, watchlist, and history. `stop` leaves start-at-login registered;
use `uninstall` to disable it permanently. A foreground `watchcatd` must be
stopped in its own terminal.

## Configuration and environment

Registration captures the current `PATH`, absolute config path, state directory,
and watchlist path, plus `CODEX_HOME` and `CLAUDE_CONFIG_DIR` when set. This lets the background server find `codex` without loading
shell startup files. Run installation from the shell where the authenticated
`codex` command works. Custom paths are supported:

```bash
WATCHCAT_STATE_DIR="$HOME/watchcat-state" \
  watchcat --config "$HOME/watchcat-config/config.toml" service install
```

Use the same config and state overrides for subsequent CLI operations. Each user
has one managed service registration. To change captured paths or `PATH`, stop
and uninstall the old registration, then install and start with the new values.
Existing service files are never overwritten by `install`. Keep the binary
location stable; don't register a disposable build directory.

## macOS launchd

The CLI writes `~/Library/LaunchAgents/ai.watchcat.watchcatd.plist`. It uses
absolute `ProgramArguments`, not an app's `BundleProgram`. `launchctl` manages
it in the current user's GUI login domain, so installation and startup should
be performed in a logged-in macOS desktop session.

Logs are `watchcatd.log` and `watchcatd.err.log` in the state directory reported
by `watchcat config path`. launchd restarts an exited server with a 10-second
throttle. Log files are not automatically rotated.

## Linux systemd user service

The CLI writes `$XDG_CONFIG_HOME/systemd/user/watchcatd.service` (normally
`~/.config/systemd/user/watchcatd.service`) and enables it for the user's login.
It requires an active systemd user manager. Inspect logs with:

```bash
journalctl --user -u watchcatd -f
```

The service uses `Restart=on-failure` with a 10-second delay. Running after
logout requires systemd user lingering configured separately by the machine's
administrator.

## Recovery control

Service start/stop is the sole global recovery control. Use
`watchcat session remove SESSION_ID` to remove a session and persistently exclude
it from automatic discovery. `watchcat session add SESSION_ID` clears that exclusion.
Provider sessions continue independently when Watchcat stops or removes an entry.

## Supported versions and platforms

Only the current configuration and state schemas are accepted; older schemas
are rejected without being rewritten. There is no direct-mode fallback or
`watchcat run` command. Runtime operations require a running `watchcatd`.
macOS and Linux are supported; Windows server transport is not implemented.
