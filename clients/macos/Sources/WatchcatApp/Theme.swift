import SwiftUI
import AppKit

enum WatchcatTheme {
    static let ink = Color(red: 0.10, green: 0.12, blue: 0.11)
    static let muted = Color(red: 0.32, green: 0.35, blue: 0.33)
    static let paper = Color(red: 0.96, green: 0.96, blue: 0.94)
    static let surface = Color(red: 0.99, green: 0.99, blue: 0.98)
    static let navy = Color(red: 0.10, green: 0.15, blue: 0.31)
    static let navySoft = Color(red: 0.86, green: 0.88, blue: 0.94)
    static let green = Color(red: 0.31, green: 0.51, blue: 0.36)
    static let sand = Color(red: 0.88, green: 0.81, blue: 0.66)
    static let line = Color.black.opacity(0.15)
}

struct QuietButtonStyle: ButtonStyle {
    var prominent = false

    func makeBody(configuration: Configuration) -> some View {
        configuration.label
            .font(.system(size: 13, weight: .semibold))
            .foregroundStyle(prominent ? .white : WatchcatTheme.ink)
            .padding(.horizontal, 14)
            .frame(minHeight: 36)
            .background(prominent ? WatchcatTheme.navy : Color.black.opacity(configuration.isPressed ? 0.09 : 0.05))
            .clipShape(RoundedRectangle(cornerRadius: 9, style: .continuous))
            .scaleEffect(configuration.isPressed ? 0.98 : 1)
            .animation(.easeOut(duration: 0.12), value: configuration.isPressed)
    }
}

struct StatusDot: View {
    let color: Color
    var body: some View { Circle().fill(color).frame(width: 8, height: 8) }
}

struct BrandLogo: View {
    var body: some View {
        if let url = Bundle.main.url(forResource: "WatchcatLogo", withExtension: "png"),
           let image = NSImage(contentsOf: url) {
            Image(nsImage: image).resizable().scaledToFit()
        } else {
            Image(systemName: "cat.fill").resizable().scaledToFit().foregroundStyle(WatchcatTheme.navy)
        }
    }
}

struct MenuBarGuardIcon: View {
    var body: some View {
        ZStack(alignment: .bottomTrailing) {
            Image(systemName: "cat.fill")
                .font(.system(size: 15, weight: .semibold))
            Image(systemName: "shield.fill")
                .font(.system(size: 7, weight: .bold))
                .offset(x: 2, y: 2)
        }
        .frame(width: 20, height: 18)
        .accessibilityLabel("Watchcat 猫保安")
    }
}

struct SectionLabel: View {
    let title: String
    let detail: String?

    init(_ title: String, detail: String? = nil) {
        self.title = title
        self.detail = detail
    }

    var body: some View {
        HStack {
            Text(title).font(.system(size: 13, weight: .semibold)).foregroundStyle(WatchcatTheme.muted)
            Spacer()
            if let detail { Text(detail).font(.system(size: 12)).foregroundStyle(WatchcatTheme.muted) }
        }
    }
}
