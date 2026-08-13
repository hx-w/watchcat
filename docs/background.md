# Running watchcatd in the background

Run `watchcat doctor` and a foreground `watchcatd --dry-run` before enabling a
service. `watchcatd` owns restarts, provider connections, hot reload, and local
RPC. The legacy `watchcat run` remains a direct foreground compatibility mode.

## macOS app

The native Watchcat app bundles `watchcatd` as an `SMAppService` LaunchAgent.
Open Connection and choose Enable. macOS may require approval in System Settings
under Login Items. The app can open that settings page directly.

The bundled LaunchAgent uses `BundleProgram`, so it continues to resolve the
helper inside the installed app after registration. Install the app in a stable
location before enabling it.

## Linux systemd user service

Create `~/.config/systemd/user/watchcatd.service`:

```ini
[Unit]
Description=Watchcat local reliability service
After=network-online.target

[Service]
ExecStart=%h/.local/bin/watchcatd
Restart=on-failure
RestartSec=10

[Install]
WantedBy=default.target
```

Then run:

```bash
systemctl --user daemon-reload
systemctl --user enable --now watchcatd
journalctl --user -u watchcatd -f
```

Stopping the service does not change the watchlist. Use
`watchcat watch remove SESSION_ID` to revoke automatic recovery authority.
