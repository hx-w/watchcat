# Running in the background

`watchcat run` is a foreground process by design. A platform service manager
should own restarts and logs. Run `watchcat doctor` and
`watchcat run --once --dry-run` successfully before enabling a service.

## Linux with systemd user services

Create `~/.config/systemd/user/watchcat.service`:

```ini
[Unit]
Description=Watchcat session watchdog
After=network-online.target

[Service]
ExecStart=%h/.local/bin/watchcat run
Restart=on-failure
RestartSec=10

[Install]
WantedBy=default.target
```

Then run:

```bash
systemctl --user daemon-reload
systemctl --user enable --now watchcat
journalctl --user -u watchcat -f
```

## macOS with launchd

Create `~/Library/LaunchAgents/ai.watchcat.watchcat.plist`. Replace
`/Users/YOU` with your home directory and use the path returned by
`command -v watchcat`.

```xml
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
  <key>Label</key><string>ai.watchcat.watchcat</string>
  <key>ProgramArguments</key>
  <array><string>/Users/YOU/.local/bin/watchcat</string><string>run</string></array>
  <key>RunAtLoad</key><true/>
  <key>KeepAlive</key><true/>
  <key>StandardOutPath</key><string>/Users/YOU/Library/Logs/watchcat.log</string>
  <key>StandardErrorPath</key><string>/Users/YOU/Library/Logs/watchcat.log</string>
</dict>
</plist>
```

Load it with:

```bash
launchctl bootstrap "gui/$(id -u)" ~/Library/LaunchAgents/ai.watchcat.watchcat.plist
```

## Windows with Task Scheduler

In an unelevated PowerShell window, replace the executable path if needed:

```powershell
$action = New-ScheduledTaskAction -Execute "$env:LOCALAPPDATA\Programs\watchcat\bin\watchcat.exe" -Argument "run"
$trigger = New-ScheduledTaskTrigger -AtLogOn
$settings = New-ScheduledTaskSettingsSet -RestartCount 10 -RestartInterval (New-TimeSpan -Minutes 1)
Register-ScheduledTask -TaskName "Watchcat" -Action $action -Trigger $trigger -Settings $settings -Description "Watchcat session watchdog"
Start-ScheduledTask -TaskName "Watchcat"
```

The watchlist remains the authorization boundary. Stopping a service does not
remove sessions from it; use `watchcat watch remove SESSION_ID` to revoke a
session.
