import SwiftUI

@main
struct WatchcatApp: App {
    @NSApplicationDelegateAdaptor(WatchcatApplicationDelegate.self) private var delegate
    @StateObject private var model = AppModel.shared

    var body: some Scene {
        Window("Watchcat", id: "main") {
            MainWindow()
                .environmentObject(model)
                .preferredColorScheme(.light)
        }
        .defaultSize(width: 980, height: 680)
        .windowToolbarStyle(.unifiedCompact(showsTitle: false))

        MenuBarExtra {
            MenuBarView()
                .environmentObject(model)
                .preferredColorScheme(.light)
        } label: {
            MenuBarGuardIcon()
        }
        .menuBarExtraStyle(.window)
    }
}
