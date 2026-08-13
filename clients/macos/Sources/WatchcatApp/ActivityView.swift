import SwiftUI

struct ActivityView: View {
    @EnvironmentObject private var model: AppModel

    var body: some View {
        VStack(spacing: 0) {
            HStack(alignment: .top) {
                VStack(alignment: .leading, spacing: 5) {
                    Text("活动").font(.system(size: 26, weight: .bold))
                    Text("按 Session 查看异常判定与 Watchcat 恢复记录。")
                        .font(.system(size: 13))
                        .foregroundStyle(WatchcatTheme.muted)
                }
                Spacer()
                Picker("Session", selection: $model.selectedSessionID) {
                    Text("选择 Session").tag(Optional<String>.none)
                    ForEach(model.watchlist) { target in
                        Text(target.displayName).tag(Optional(target.sessionID))
                    }
                }
                .frame(width: 280)
                .onChange(of: model.selectedSessionID) { value in
                    if let value { Task { await model.loadActivity(sessionID: value) } }
                }
            }
            .padding(24)
            if model.selectedSessionID == nil {
                VStack(spacing: 10) {
                    Image(systemName: "clock.arrow.circlepath")
                        .font(.system(size: 34))
                        .foregroundStyle(WatchcatTheme.muted)
                    Text("选择一个 Session").font(.system(size: 16, weight: .semibold))
                    Text("请从上方选择要查看的 Session。")
                        .font(.system(size: 12))
                        .foregroundStyle(WatchcatTheme.muted)
                }
                .frame(maxWidth: .infinity, maxHeight: .infinity)
            } else if model.isActivityLoading {
                VStack(spacing: 9) {
                    ProgressView()
                        .controlSize(.small)
                    Text("正在读取异常记录")
                        .font(.system(size: 12, weight: .medium))
                        .foregroundStyle(WatchcatTheme.muted)
                }
                .frame(maxWidth: .infinity, maxHeight: .infinity)
            } else if model.activity.isEmpty {
                VStack(spacing: 9) {
                    Image(systemName: "checkmark.shield")
                        .font(.system(size: 32))
                        .foregroundStyle(WatchcatTheme.muted)
                    Text("暂无需要关注的活动")
                        .font(.system(size: 15, weight: .semibold))
                    Text("普通对话和正常回合不会显示在这里。")
                        .font(.system(size: 11))
                        .foregroundStyle(WatchcatTheme.muted)
                }
                .frame(maxWidth: .infinity, maxHeight: .infinity)
            } else {
                List(model.activity) { entry in
                    VStack(alignment: .leading, spacing: 5) {
                        HStack {
                            Text(activityTitle(entry)).font(.system(size: 13, weight: .semibold))
                            Spacer()
                            Text(entry.timestamp?.formatted(date: .omitted, time: .shortened) ?? "")
                                .font(.system(size: 11))
                                .foregroundStyle(WatchcatTheme.muted)
                        }
                        Text(entry.message).font(.system(size: 12)).foregroundStyle(WatchcatTheme.ink)
                        Text([entry.condition, entry.kind].compactMap { $0 }.joined(separator: " · "))
                            .font(.system(size: 10, design: .monospaced))
                            .foregroundStyle(WatchcatTheme.muted)
                    }
                    .padding(.vertical, 6)
                }
                .listStyle(.inset)
            }
        }
        .background(WatchcatTheme.paper)
    }

    private func activityTitle(_ entry: SessionLog) -> String {
        switch entry.kind {
        case "recovery.completed": "自动恢复成功"
        case "recovery.failed": "恢复仍未完成"
        case "retry.sent": "已发送恢复指令"
        case "retry.waiting": "正在按策略等待"
        case "retry.failed": "恢复指令发送失败"
        case "retry.exhausted": "已达到重试上限"
        case "retry.cancelled": "Session 已变化，取消恢复"
        case "retry.dry_run": "已模拟恢复动作"
        case "failure.skipped": "策略决定不重试"
        case "turn.failed": "Session 发生异常"
        case "provider.error": "无法读取 Session 状态"
        default: entry.kind.replacingOccurrences(of: ".", with: " ")
        }
    }
}
