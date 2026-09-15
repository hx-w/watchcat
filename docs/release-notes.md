Watchcat 0.6.0 adds event-driven handling of recognized macOS directory and
volume permission dialogs. While the service runs, it subscribes to system
application and Accessibility events and presses Allow on supported requests,
including removable and network volumes. No new configuration or periodic
desktop scan is needed. Service status shows readiness and the latest result.

Grant the installed `watchcatd` executable Accessibility access once in System
Settings, then restart the service. This applies across the current desktop,
including apps outside the session watchlist. Recognition uses known English
and Chinese directory-consent headings; passwords and unrelated permission
categories are excluded. Live macOS permission prompts have not yet been
verified, and other languages or system dialog layouts may remain unhandled.

Upgrade `watchcat` and `watchcatd` together. Existing 0.5.0 configuration and state
remain compatible, and Linux behavior is unchanged. Stopping the service stops
further clicks; permissions already granted remain granted.

**Full Changelog**: https://github.com/hx-w/watchcat/compare/v0.5.0...v0.6.0
