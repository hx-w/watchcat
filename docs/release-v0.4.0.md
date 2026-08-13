Watchcat 0.4 introduces the native macOS menu-bar client and the long-running
local Watchcat service. The client manages guarded sessions, recovery policies,
reliability activity, lifecycle cleanup, and launch-at-login from one compact
interface.

The macOS app archives are preview builds: they are ad-hoc signed but not Apple
notarized. Choose the archive for your Mac, then follow the included README for
the one-time Privacy & Security approval. CLI and service archives remain
checksum-verified release builds.

Upgrade the CLI and service together. Watchcat 0.4 automatically migrates v0.3
configuration, but Watchcat 0.3 cannot read configuration after that migration.

**Full Changelog**: https://github.com/hx-w/watchcat/compare/v0.3.1...v0.4.0
