import AppKit

final class WatchcatApplicationDelegate: NSObject, NSApplicationDelegate {
    @MainActor
    static func bringMainWindowForward() {
        NSApp.activate(ignoringOtherApps: true)
        focusMainWindow(after: 0.06)
        focusMainWindow(after: 0.24)
    }

    @MainActor
    private static func focusMainWindow(after delay: TimeInterval) {
        DispatchQueue.main.asyncAfter(deadline: .now() + delay) {
            let mainWindow = NSApp.windows.first {
                $0.title == "Watchcat" && $0.frame.width >= 800 && $0.canBecomeKey
            }
            if mainWindow?.isMiniaturized == true {
                mainWindow?.deminiaturize(nil)
            }
            mainWindow?.makeKeyAndOrderFront(nil)
            mainWindow?.orderFrontRegardless()
        }
    }
}
