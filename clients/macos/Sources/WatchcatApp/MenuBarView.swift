import AppKit
import SwiftUI

struct MenuBarView: View {
    @EnvironmentObject private var model: AppModel
    @Environment(\.openWindow) private var openWindow
    @State private var page = 0
    @State private var hoveredTargetID: String?
    @State private var pendingRemoval: WatchTarget?
    private var attentionTarget: WatchTarget? {
        model.watchlist.first(where: { model.snapshot.attentionTargetKeys.contains($0.id) })
    }

    private var menuTargets: [WatchTarget] {
        var targets = model.watchlist.filter { model.snapshot.attentionTargetKeys.contains($0.id) }
        targets.append(contentsOf: model.watchlist.filter { $0.enabled && !targets.contains($0) })
        targets.append(contentsOf: model.watchlist.filter { !$0.enabled && !targets.contains($0) })
        return targets
    }

    private var pageCount: Int {
        WatchlistPaging.pageCount(itemCount: menuTargets.count)
    }

    private var visibleTargets: [WatchTarget] {
        WatchlistPaging.items(menuTargets, page: page)
    }

    var body: some View {
        ZStack {
            VStack(spacing: 0) {
                header
                Divider().opacity(0.55)
                statusSummary
                guardedSessions
                Divider().opacity(0.55)
                footer
            }
            if let target = pendingRemoval {
                removalConfirmation(for: target)
                    .transition(.opacity.combined(with: .scale(scale: 0.97)))
                    .zIndex(1)
            }
        }
        .frame(width: 370)
        .foregroundStyle(WatchcatTheme.ink)
        .tint(WatchcatTheme.navy)
        .background(WatchcatTheme.paper)
        .onChange(of: model.watchlist.count) { _ in
            page = WatchlistPaging.clampedPage(page, itemCount: menuTargets.count)
        }
    }

    private func removalConfirmation(for target: WatchTarget) -> some View {
        ZStack {
            Color.black.opacity(0.16)
                .contentShape(Rectangle())
                .onTapGesture { cancelRemoval() }

            VStack(alignment: .leading, spacing: 10) {
                Text("移出守护列表？")
                    .font(.system(size: 15, weight: .bold))
                Text("将停止守护“\(target.displayName)”，不会删除原始 Session。")
                    .font(.system(size: 12))
                    .foregroundStyle(WatchcatTheme.muted)
                    .fixedSize(horizontal: false, vertical: true)
                HStack(spacing: 8) {
                    Button("取消") { cancelRemoval() }
                        .buttonStyle(QuietButtonStyle())
                    Button("移出") { confirmRemoval(target) }
                        .buttonStyle(MenuRemovalButtonStyle())
                }
            }
            .padding(16)
            .frame(width: 286, alignment: .leading)
            .background(WatchcatTheme.surface)
            .clipShape(RoundedRectangle(cornerRadius: 15, style: .continuous))
            .shadow(color: .black.opacity(0.16), radius: 8, y: 3)
        }
        .accessibilityElement(children: .contain)
        .accessibilityLabel("确认移出守护列表")
    }

    private func cancelRemoval() {
        withAnimation(.easeOut(duration: 0.12)) {
            pendingRemoval = nil
        }
    }

    private func confirmRemoval(_ target: WatchTarget) {
        withAnimation(.easeOut(duration: 0.12)) {
            pendingRemoval = nil
        }
        Task { await model.remove(target) }
    }

    private var header: some View {
        HStack(spacing: 11) {
            BrandLogo()
                .frame(width: 40, height: 40)
            VStack(alignment: .leading, spacing: 3) {
                Text("Watchcat").font(.system(size: 18, weight: .bold))
                HStack(spacing: 6) {
                    StatusDot(color: model.snapshot.serviceOnline ? WatchcatTheme.green : .red)
                    Text(model.snapshot.serviceOnline ? "守护服务在线" : "守护服务离线")
                        .font(.system(size: 12))
                        .foregroundStyle(WatchcatTheme.muted)
                }
                if let until = model.snapshot.guardPausedUntil {
                    Text("暂停至 \(until.formatted(date: .omitted, time: .shortened))")
                        .font(.system(size: 10))
                        .foregroundStyle(WatchcatTheme.muted)
                }
            }
            Spacer()
            Button {
                Task {
                    model.snapshot.guardEnabled ? await model.pauseGuard() : await model.setGuard(true)
                }
            } label: {
                HStack(spacing: 9) {
                    Text(model.snapshot.guardEnabled ? "托管中" : "已暂停")
                        .font(.system(size: 13, weight: .semibold))
                    ZStack(alignment: model.snapshot.guardEnabled ? .trailing : .leading) {
                        Capsule()
                            .fill(model.snapshot.guardEnabled ? WatchcatTheme.green : Color.black.opacity(0.12))
                            .frame(width: 45, height: 26)
                        Circle()
                            .fill(.white)
                            .frame(width: 20, height: 20)
                            .padding(3)
                    }
                }
                .foregroundStyle(WatchcatTheme.ink)
            }
            .buttonStyle(.plain)
            .disabled(!model.snapshot.serviceOnline)
            .accessibilityLabel("自动托管")
            .accessibilityValue(model.snapshot.guardEnabled ? "已开启" : "已暂停")
        }
        .padding(18)
    }

    private var statusSummary: some View {
        VStack(alignment: .leading, spacing: 11) {
            HStack(alignment: .firstTextBaseline) {
                Text(model.snapshot.attention == 0 ? "守护正常" : "\(model.snapshot.attention) 个会话需要留意")
                    .font(.system(size: 17, weight: .bold))
                Spacer()
                Text("守护 \(model.snapshot.watched) 个 Session")
                    .font(.system(size: 12))
                    .foregroundStyle(WatchcatTheme.muted)
            }
            if model.snapshot.attention > 0 {
                Text("已按策略处理失败，可提前重试或查看活动。")
                    .font(.system(size: 12))
            }
            Divider()
            HStack(spacing: 18) {
                Text("\(model.snapshot.automaticRecoveries) 次自动恢复")
                Text("\(model.snapshot.handsFreePercent)% 无需人工介入")
            }
            .font(.system(size: 13, weight: .semibold))
        }
        .padding(15)
        .background(WatchcatTheme.surface)
        .clipShape(RoundedRectangle(cornerRadius: 14, style: .continuous))
        .overlay(RoundedRectangle(cornerRadius: 14).stroke(WatchcatTheme.line))
        .padding(.horizontal, 16)
        .padding(.top, 14)
        .padding(.bottom, 4)
    }

    private var guardedSessions: some View {
        VStack(alignment: .leading, spacing: 8) {
            SectionLabel(
                model.snapshot.attention > 0 ? "需要处理" : "守护 Session",
                detail: model.watchlist.isEmpty ? nil : "共 \(model.watchlist.count) 个"
            )
            if model.watchlist.isEmpty {
                VStack(alignment: .leading, spacing: 4) {
                    Text("还没有守护 Session")
                        .font(.system(size: 13, weight: .semibold))
                    Text("从“守护”中添加需要自动恢复的任务。")
                        .font(.system(size: 11))
                        .foregroundStyle(WatchcatTheme.muted)
                }
                .frame(maxWidth: .infinity, alignment: .leading)
                .padding(14)
                .background(WatchcatTheme.surface)
                .clipShape(RoundedRectangle(cornerRadius: 12))
                .overlay(RoundedRectangle(cornerRadius: 12).stroke(WatchcatTheme.line))
            } else {
                VStack(spacing: 0) {
                    ForEach(Array(visibleTargets.enumerated()), id: \.element.id) { index, target in
                        sessionRow(target)
                        if index < visibleTargets.count - 1 {
                            Divider().padding(.leading, 57)
                        }
                    }
                }
                .background(WatchcatTheme.surface)
                .clipShape(RoundedRectangle(cornerRadius: 12))
                .overlay(RoundedRectangle(cornerRadius: 12).stroke(WatchcatTheme.line))
                if pageCount > 1 {
                    pagination
                }
            }
        }
        .padding(.horizontal, 16)
        .padding(.vertical, 12)
    }

    private func sessionRow(_ target: WatchTarget) -> some View {
        HStack(spacing: 11) {
            Text(target.provider == "codex" ? "Cx" : "Cl")
                .font(.system(size: 12, weight: .bold))
                .foregroundStyle(.white)
                .frame(width: 32, height: 32)
                .background(WatchcatTheme.navy)
                .clipShape(RoundedRectangle(cornerRadius: 8))
            VStack(alignment: .leading, spacing: 3) {
                Text(target.displayName)
                    .font(.system(size: 13, weight: .semibold))
                    .lineLimit(1)
                Text(sessionStatus(target))
                    .font(.system(size: 11))
                    .foregroundStyle(WatchcatTheme.muted)
            }
            Spacer(minLength: 8)
            if hoveredTargetID == target.id {
                HStack(spacing: 2) {
                    if target == attentionTarget {
                        compactAction("arrow.clockwise", help: "立即重试") {
                            Task { await model.retryNow(target) }
                        }
                    }
                    compactAction(target.enabled ? "pause.fill" : "play.fill", help: target.enabled ? "暂停守护" : "恢复守护") {
                        Task { await model.toggle(target) }
                    }
                    compactAction("trash", help: "移出守护列表", destructive: true) {
                        pendingRemoval = target
                    }
                }
            } else {
                StatusDot(color: target.enabled ? WatchcatTheme.green : WatchcatTheme.muted.opacity(0.5))
                    .padding(.horizontal, 9)
            }
        }
        .padding(.horizontal, 14)
        .frame(minHeight: 58)
        .contentShape(Rectangle())
        .background(hoveredTargetID == target.id ? Color.black.opacity(0.025) : .clear)
        .onHover { hovering in
            hoveredTargetID = hovering ? target.id : nil
        }
    }

    private func compactAction(
        _ systemImage: String,
        help: String,
        destructive: Bool = false,
        action: @escaping () -> Void
    ) -> some View {
        Button(action: action) {
            Image(systemName: systemImage)
                .font(.system(size: 11, weight: .semibold))
                .frame(width: 32, height: 32)
                .contentShape(Rectangle())
        }
        .buttonStyle(MenuRowActionStyle(destructive: destructive))
        .help(help)
        .accessibilityLabel(help)
    }

    private var pagination: some View {
        HStack(spacing: 8) {
            Button { page = max(0, page - 1) } label: {
                Image(systemName: "chevron.left").frame(width: 32, height: 28)
            }
            .buttonStyle(.plain)
            .disabled(page == 0)
            .accessibilityLabel("上一页")

            Text("\(page + 1) / \(pageCount)")
                .font(.system(size: 11, weight: .medium))
                .foregroundStyle(WatchcatTheme.muted)
                .monospacedDigit()

            Button { page = min(pageCount - 1, page + 1) } label: {
                Image(systemName: "chevron.right").frame(width: 32, height: 28)
            }
            .buttonStyle(.plain)
            .disabled(page >= pageCount - 1)
            .accessibilityLabel("下一页")
        }
        .frame(maxWidth: .infinity)
        .padding(.top, 4)
    }

    private var footer: some View {
        VStack(spacing: 9) {
            HStack(spacing: 8) {
                Button {
                    showMainWindow(.watchlist)
                } label: {
                    Label("守护", systemImage: "shield")
                        .frame(maxWidth: .infinity)
                }
                .buttonStyle(QuietButtonStyle())

                Button {
                    showMainWindow(.policies)
                } label: {
                    Label("策略", systemImage: "slider.horizontal.3")
                        .frame(maxWidth: .infinity)
                }
                .buttonStyle(QuietButtonStyle())
            }
            HStack {
                Text("\(model.snapshot.watched) 个守护 · \(model.snapshot.paused) 个暂停")
                    .font(.system(size: 11)).foregroundStyle(WatchcatTheme.muted)
                Spacer()
                Text(model.snapshot.serviceOnline ? "配置已同步" : "等待连接")
                    .font(.system(size: 10))
                    .foregroundStyle(WatchcatTheme.muted)
            }
        }
        .padding(16)
    }

    private func sessionStatus(_ target: WatchTarget) -> String {
        if model.snapshot.attentionTargetKeys.contains(target.id) { return "检测到失败，已按策略处理" }
        if !target.enabled { return "自动恢复已暂停" }
        if let date = target.lastEventAt {
            return "最近活动 \(date.formatted(.relative(presentation: .named)))"
        }
        return "等待 Session 事件"
    }

    private func showMainWindow(_ section: MainSection) {
        model.mainSection = section
        openWindow(id: "main")
        WatchcatApplicationDelegate.bringMainWindowForward()
    }
}

private struct MenuRowActionStyle: ButtonStyle {
    let destructive: Bool

    func makeBody(configuration: Configuration) -> some View {
        configuration.label
            .foregroundStyle(destructive ? Color.red.opacity(0.82) : WatchcatTheme.ink)
            .background(Color.black.opacity(configuration.isPressed ? 0.10 : 0.045))
            .clipShape(RoundedRectangle(cornerRadius: 7, style: .continuous))
            .scaleEffect(configuration.isPressed ? 0.95 : 1)
            .animation(.easeOut(duration: 0.10), value: configuration.isPressed)
    }
}

private struct MenuRemovalButtonStyle: ButtonStyle {
    func makeBody(configuration: Configuration) -> some View {
        configuration.label
            .font(.system(size: 12, weight: .semibold))
            .foregroundStyle(Color(red: 0.72, green: 0.18, blue: 0.14))
            .frame(maxWidth: .infinity)
            .frame(height: 34)
            .background(Color(red: 0.96, green: 0.86, blue: 0.84))
            .clipShape(RoundedRectangle(cornerRadius: 9, style: .continuous))
            .opacity(configuration.isPressed ? 0.78 : 1)
    }
}
