import SwiftUI

enum MainSection: String, CaseIterable, Identifiable {
    case watchlist = "守护列表"
    case policies = "策略"
    case activity = "活动"
    case connection = "连接"

    var id: String { rawValue }
    var icon: String {
        switch self {
        case .watchlist: "shield"
        case .policies: "slider.horizontal.3"
        case .activity: "clock.arrow.circlepath"
        case .connection: "point.3.connected.trianglepath.dotted"
        }
    }
}

struct MainWindow: View {
    @EnvironmentObject private var model: AppModel

    var body: some View {
        HStack(spacing: 0) {
            sidebar
                .frame(width: 205)
            Divider()
            switch model.mainSection {
            case .watchlist: WatchlistView()
            case .policies: PoliciesView()
            case .activity: ActivityView()
            case .connection: ConnectionView()
            }
        }
        .tint(WatchcatTheme.navy)
        .foregroundStyle(WatchcatTheme.ink)
        .frame(minWidth: 920, minHeight: 620)
        .overlay(alignment: .bottom) {
            if let error = model.errorMessage {
                HStack(spacing: 9) {
                    Image(systemName: "exclamationmark.triangle.fill").foregroundStyle(.orange)
                    Text(error).font(.system(size: 12)).lineLimit(2)
                    Spacer()
                    Button("关闭") { model.errorMessage = nil }.buttonStyle(.plain)
                }
                .padding(12)
                .background(WatchcatTheme.surface)
                .overlay(alignment: .top) { Divider() }
            }
        }
    }

    private var sidebar: some View {
        VStack(spacing: 0) {
            HStack(spacing: 9) {
                BrandLogo().frame(width: 32, height: 32)
                Text("Watchcat").font(.system(size: 16, weight: .bold))
                Spacer()
            }
            .padding(16)
            VStack(spacing: 4) {
                ForEach(MainSection.allCases) { section in
                    Button {
                        model.mainSection = section
                    } label: {
                        HStack(spacing: 10) {
                            Image(systemName: section.icon)
                                .frame(width: 18)
                            Text(section.rawValue)
                            Spacer(minLength: 0)
                        }
                        .font(.system(size: 13, weight: model.mainSection == section ? .semibold : .regular))
                        .foregroundStyle(WatchcatTheme.ink)
                        .padding(.horizontal, 11)
                        .frame(maxWidth: .infinity, minHeight: 40, alignment: .leading)
                        .contentShape(Rectangle())
                    }
                    .buttonStyle(SidebarItemStyle(selected: model.mainSection == section))
                    .accessibilityValue(model.mainSection == section ? "已选择" : "")
                }
            }
            .padding(.horizontal, 10)
            Spacer()
            VStack(alignment: .leading, spacing: 3) {
                HStack(spacing: 6) {
                    StatusDot(color: model.snapshot.serviceOnline ? WatchcatTheme.green : .red)
                    Text(model.snapshot.serviceOnline ? "守护服务在线" : "守护服务离线")
                        .font(.system(size: 11, weight: .semibold))
                }
                Text(model.snapshot.serviceOnline ? "配置已同步" : "等待连接")
                    .font(.system(size: 10))
                    .foregroundStyle(WatchcatTheme.muted)
            }
            .padding(16)
            .frame(maxWidth: .infinity, alignment: .leading)
        }
        .background(Color(red: 0.91, green: 0.92, blue: 0.89))
    }
}

private struct SidebarItemStyle: ButtonStyle {
    let selected: Bool

    func makeBody(configuration: Configuration) -> some View {
        configuration.label
            .background(
                selected
                    ? Color.white.opacity(configuration.isPressed ? 0.68 : 0.84)
                    : Color.black.opacity(configuration.isPressed ? 0.055 : 0)
            )
            .clipShape(RoundedRectangle(cornerRadius: 8, style: .continuous))
    }
}
