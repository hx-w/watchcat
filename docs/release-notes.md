Watchcat 0.7.0 adds configurable macOS window click rules through
`watchcat config dialog`. Match an app, window title or primary text, and button;
changes take effect immediately. The built-in directory and volume permission
rule remains enabled by default. `watchcat service logs` shows persistent events
with local timestamps, app, rule, button, and click outcomes; use `--clicks` to
filter actions or `--json` for structured output.

The monitor now observes new background and LSUIElement applications as well as
ordinary app launches. Transient Accessibility subscription failures receive
bounded retries, and activation or wake events can recover unavailable hosts.
Discovery uses event subscriptions without periodic desktop scans. Interrupted
log writes no longer corrupt subsequent records.

Upgrade `watchcat` and `watchcatd` together. Existing 0.6.0 configuration and state
remain compatible, and Linux behavior is unchanged. Configuration containing the
new `[dialogs]` section requires 0.7.0 or later; restore an older configuration
backup before downgrading. Check Accessibility access for the installed
`watchcatd` after replacing it, then restart the service. Live macOS permission
prompts have not yet been verified; unrecognized dialog layouts remain unhandled.

**Full Changelog**: https://github.com/hx-w/watchcat/compare/v0.6.0...v0.7.0
