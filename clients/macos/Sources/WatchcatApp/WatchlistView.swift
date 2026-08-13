import SwiftUI

enum WatchFilter: String, CaseIterable, Identifiable {
    case all = "全部"
    case attention = "需留意"
    case active = "运行中"
    case paused = "已暂停"
    case expiring = "即将移出"
    var id: String { rawValue }
}

struct WatchlistView: View {
    @EnvironmentObject private var model: AppModel
    @State private var filter = WatchFilter.all
    @State private var showDiscovery = false
    @State private var pendingRemoval: WatchTarget?
    @State private var pendingInterrupt: WatchTarget?
    @State private var showLifecycle = false

    private var filtered: [WatchTarget] {
        model.watchlist.filter { target in
            let matches = model.search.isEmpty
                || target.displayName.localizedCaseInsensitiveContains(model.search)
                || target.sessionID.localizedCaseInsensitiveContains(model.search)
            guard matches else { return false }
            return switch filter {
            case .all: true
            case .attention: model.snapshot.attentionTargetKeys.contains(target.id)
            case .active: target.enabled
            case .paused: !target.enabled
            case .expiring:
                !target.protected && (target.lastEventAt ?? target.addedAt) < Date.now.addingTimeInterval(-2 * 86_400)
            }
        }
    }

    var body: some View {
        VStack(spacing: 0) {
            pageHeader
            filterBar
            if showLifecycle { lifecycleBar.transition(.opacity.combined(with: .move(edge: .top))) }
            List(filtered) { target in
                watchRow(target)
                    .listRowSeparator(.visible)
                    .listRowBackground(WatchcatTheme.surface)
            }
            .listStyle(.inset)
        }
        .background(WatchcatTheme.paper)
        .sheet(isPresented: $showDiscovery) { DiscoverySheet() }
        .confirmationDialog(
            "从守护列表移出？",
            isPresented: Binding(
                get: { pendingRemoval != nil },
                set: { if !$0 { pendingRemoval = nil } }
            ),
            titleVisibility: .visible
        ) {
            Button("移出", role: .destructive) {
                guard let target = pendingRemoval else { return }
                Task { await model.remove(target) }
            }
            Button("取消", role: .cancel) {}
        } message: {
            Text("只停止 Watchcat 守护，不会删除 Provider 中的 Session。")
        }
        .confirmationDialog(
            "中断正在执行的回合？",
            isPresented: Binding(
                get: { pendingInterrupt != nil },
                set: { if !$0 { pendingInterrupt = nil } }
            ),
            titleVisibility: .visible
        ) {
            Button("中断回合", role: .destructive) {
                guard let target = pendingInterrupt else { return }
                pendingInterrupt = nil
                Task { await model.interrupt(target) }
            }
            Button("取消", role: .cancel) {}
        } message: {
            Text("这会立即停止 Provider 中当前正在执行的 Agent 回合。")
        }
    }

    private var pageHeader: some View {
        HStack(alignment: .firstTextBaseline) {
            VStack(alignment: .leading, spacing: 5) {
                Text("守护列表").font(.system(size: 26, weight: .bold))
                Text("管理 Watchcat 负责恢复的 Session。")
                    .font(.system(size: 13))
                    .foregroundStyle(WatchcatTheme.muted)
            }
            Spacer()
            Button {
                withAnimation(.easeOut(duration: 0.16)) { showLifecycle.toggle() }
            } label: {
                Label("自动整理", systemImage: "clock.arrow.2.circlepath")
            }
            .buttonStyle(QuietButtonStyle())
            Button("添加 Session") { showDiscovery = true }
                .buttonStyle(QuietButtonStyle(prominent: true))
        }
        .padding(24)
    }

    private var lifecycleBar: some View {
        VStack(alignment: .leading, spacing: 17) {
            VStack(alignment: .leading, spacing: 5) {
                Text("自动整理历史守护")
                    .font(.system(size: 14, weight: .semibold))
                Text("按最近事件清理过期守护，不删除原始 Session。")
                    .font(.system(size: 11))
                    .foregroundStyle(WatchcatTheme.muted)
            }

            Divider()

            HStack(spacing: 24) {
                HStack(spacing: 12) {
                    VStack(alignment: .leading, spacing: 3) {
                        Text("无新事件")
                            .font(.system(size: 12, weight: .semibold))
                        Text("到期后自动移出守护列表")
                            .font(.system(size: 10))
                            .foregroundStyle(WatchcatTheme.muted)
                    }
                    Spacer(minLength: 18)
                    Picker("无新事件", selection: Binding(
                        get: { model.lifecycle.staleAfterSeconds },
                        set: { value in model.lifecycle.staleAfterSeconds = value; Task { await model.saveLifecycle() } }
                    )) {
                        Text("1 天后").tag(86_400)
                        Text("3 天后").tag(259_200)
                        Text("7 天后").tag(604_800)
                    }
                    .labelsHidden()
                    .pickerStyle(.menu)
                    .frame(width: 112)
                }
                .frame(maxWidth: .infinity)

                Divider()
                    .frame(height: 38)

                HStack(spacing: 12) {
                    VStack(alignment: .leading, spacing: 3) {
                        Text("未解决异常")
                            .font(.system(size: 12, weight: .semibold))
                        Text("有待处理异常时保留守护")
                            .font(.system(size: 10))
                            .foregroundStyle(WatchcatTheme.muted)
                    }
                    Spacer(minLength: 18)
                    Toggle("保护未解决异常", isOn: Binding(
                        get: { model.lifecycle.protectUnresolvedFailures },
                        set: { value in model.lifecycle.protectUnresolvedFailures = value; Task { await model.saveLifecycle() } }
                    ))
                    .labelsHidden()
                    .toggleStyle(.switch)
                }
                .frame(maxWidth: .infinity)
            }
        }
        .padding(.horizontal, 20)
        .padding(.vertical, 18)
        .background(WatchcatTheme.surface)
        .clipShape(RoundedRectangle(cornerRadius: 12, style: .continuous))
        .overlay(
            RoundedRectangle(cornerRadius: 12, style: .continuous)
                .stroke(WatchcatTheme.line)
        )
        .padding(.horizontal, 24)
        .padding(.top, 12)
        .padding(.bottom, 18)
    }

    private var filterBar: some View {
        HStack(spacing: 12) {
            TextField("搜索名称或 Session ID", text: $model.search)
                .textFieldStyle(.roundedBorder)
            Menu {
                ForEach(WatchFilter.allCases) { item in
                    Button(item.rawValue) { filter = item }
                }
            } label: {
                HStack(spacing: 5) {
                    Text(filter.rawValue)
                    Image(systemName: "chevron.down")
                        .font(.system(size: 9, weight: .semibold))
                }
                .foregroundStyle(WatchcatTheme.ink)
            }
            .menuStyle(.borderlessButton)
            .frame(width: 90)
        }
        .padding(24)
        .padding(.bottom, 0)
    }

    private func watchRow(_ target: WatchTarget) -> some View {
        HStack(spacing: 13) {
            Text(target.provider == "codex" ? "Cx" : "Cl")
                .font(.system(size: 13, weight: .bold))
                .foregroundStyle(.white)
                .frame(width: 38, height: 38)
                .background(WatchcatTheme.navy)
                .clipShape(RoundedRectangle(cornerRadius: 9))
            VStack(alignment: .leading, spacing: 4) {
                HStack(spacing: 6) {
                    Text(target.displayName).font(.system(size: 14, weight: .semibold))
                    if target.protected {
                        Image(systemName: "lock.fill").font(.system(size: 10)).foregroundStyle(WatchcatTheme.muted)
                    }
                }
                Text(target.sessionID).font(.system(size: 11, design: .monospaced)).foregroundStyle(WatchcatTheme.muted).lineLimit(1)
            }
            Spacer()
            StatusDot(color: target.enabled ? WatchcatTheme.green : WatchcatTheme.muted.opacity(0.5))
            Text(target.enabled ? "守护中" : "已暂停").font(.system(size: 12)).foregroundStyle(WatchcatTheme.muted)
            Menu {
                if model.snapshot.attentionTargetKeys.contains(target.id) {
                    Button("立即重试") { Task { await model.retryNow(target) } }
                }
                Button("查看活动") {
                    model.mainSection = .activity
                    Task { await model.loadActivity(sessionID: target.sessionID) }
                }
                Button("中断活动回合", role: .destructive) { pendingInterrupt = target }
                Divider()
                Button(target.enabled ? "暂停守护" : "恢复守护") { Task { await model.toggle(target) } }
                Button(target.protected ? "取消长期保护" : "长期保护") { Task { await model.setProtected(target, protected: !target.protected) } }
                Divider()
                Button("移出守护列表", role: .destructive) { pendingRemoval = target }
            } label: {
                Image(systemName: "ellipsis").frame(width: 36, height: 36)
            }
            .menuStyle(.borderlessButton)
            .menuIndicator(.hidden)
            .accessibilityLabel("更多操作")
        }
        .padding(.vertical, 8)
    }

}

private struct DiscoverySheet: View {
    @EnvironmentObject private var model: AppModel
    @Environment(\.dismiss) private var dismiss

    var body: some View {
        VStack(spacing: 0) {
            HStack {
                VStack(alignment: .leading, spacing: 4) {
                    Text("添加 Session").font(.system(size: 20, weight: .bold))
                    Text("优先显示最近活动，由 Watchcat 服务完成搜索。")
                        .font(.system(size: 12))
                        .foregroundStyle(WatchcatTheme.muted)
                }
                Spacer()
                Button("完成") { dismiss() }.buttonStyle(QuietButtonStyle())
            }
            .padding(20)
            TextField("搜索 Session", text: $model.search)
                .textFieldStyle(.roundedBorder)
                .padding(.horizontal, 20)
                .onSubmit { Task { await model.searchSessions(reset: true) } }
            List(model.sessions) { envelope in
                HStack {
                    VStack(alignment: .leading, spacing: 3) {
                        Text(envelope.session.title).font(.system(size: 13, weight: .semibold))
                        Text(envelope.session.id).font(.system(size: 10, design: .monospaced)).foregroundStyle(WatchcatTheme.muted)
                    }
                    Spacer()
                    if envelope.watched {
                        Text("已守护").font(.system(size: 11)).foregroundStyle(WatchcatTheme.muted)
                    } else {
                        Button("添加") { Task { await model.add(envelope.session) } }
                            .buttonStyle(QuietButtonStyle())
                    }
                }
                .padding(.vertical, 5)
            }
            .listStyle(.inset)
            if model.sessionsHasMore {
                Button("加载更多") { Task { await model.searchSessions(reset: false) } }
                    .buttonStyle(QuietButtonStyle())
                    .padding(.bottom, 16)
            }
        }
        .frame(width: 620, height: 520)
        .task { await model.searchSessions(reset: true) }
    }
}
