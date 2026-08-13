# Install the macOS preview

This Watchcat app is ad-hoc signed but not notarized by Apple. The build is
published from the Watchcat GitHub Actions release workflow with a SHA-256
checksum. macOS may block the first launch because there is no Developer ID
signature yet.

1. Move `Watchcat.app` to `Applications`.
2. In Finder, Control-click Watchcat and choose **Open**.
3. If macOS still blocks it, open **System Settings → Privacy & Security**, find
   the Watchcat message, choose **Open Anyway**, then confirm **Open**.
4. In Watchcat, open **Connection** and enable launch at login if desired.

## Update the preview

1. Open **Connection** and disable launch at login.
2. Quit Watchcat with Command-Q.
3. Replace the app in `Applications` with the new archive, reopen it, and
   enable launch at login again.

This restarts the bundled service with the same version as the client. Do not
mix a new client with a service left running from an older app bundle.

Only use the archive downloaded from the official `hx-w/watchcat` GitHub
Release and verify it against `SHA256SUMS`. A notarized build will replace this
manual first-launch flow after the project obtains an Apple Developer ID.
