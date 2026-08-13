import SwiftUI

struct ConnectionView: View {
    @EnvironmentObject private var model: AppModel

    var body: some View {
        VStack(alignment: .leading, spacing: 22) {
            VStack(alignment: .leading, spacing: 5) {
                Text("连接").font(.system(size: 26, weight: .bold))
                Text("客户端只连接当前用户的本机守护服务，不开放网络端口。")
                    .font(.system(size: 13))
                    .foregroundStyle(WatchcatTheme.muted)
            }
            VStack(spacing: 0) {
                connectionRow("Watchcat 服务", detail: model.snapshot.serviceOnline ? "本机连接已建立" : "当前未连接", color: model.snapshot.serviceOnline ? WatchcatTheme.green : .red)
                Divider()
                connectionRow("Codex", detail: model.snapshot.serviceOnline ? "连接由 Watchcat 服务管理" : "等待守护服务", color: model.snapshot.serviceOnline ? WatchcatTheme.green : WatchcatTheme.muted)
            }
            Divider()
            VStack(alignment: .leading, spacing: 8) {
                Text("登录后启动").font(.system(size: 14, weight: .semibold))
                Text("后台服务状态：\(model.serviceStatus)")
                    .font(.system(size: 12))
                    .foregroundStyle(WatchcatTheme.muted)
                HStack {
                    Button("启用") { model.registerService() }.buttonStyle(QuietButtonStyle(prominent: true))
                    Button("停用") { model.unregisterService() }.buttonStyle(QuietButtonStyle())
                    Button("打开登录项设置") { model.openLoginItemsSettings() }.buttonStyle(QuietButtonStyle())
                }
                Text("启用时会把同版本命令行工具同步到 ~/.local/bin，并检查当前 PATH。")
                    .font(.system(size: 11))
                    .foregroundStyle(WatchcatTheme.muted)
            }
            Divider()
            VStack(alignment: .leading, spacing: 8) {
                Text("命令行工具").font(.system(size: 14, weight: .semibold))
                Text(model.commandLineStatus)
                    .font(.system(size: 12))
                    .foregroundStyle(WatchcatTheme.muted)
                Button("同步命令行工具") { model.syncCommandLineTools() }
                    .buttonStyle(QuietButtonStyle())
            }
            Spacer()
        }
        .padding(24)
        .background(WatchcatTheme.paper)
    }

    private func connectionRow(_ name: String, detail: String, color: Color) -> some View {
        HStack(spacing: 12) {
            StatusDot(color: color)
            VStack(alignment: .leading, spacing: 3) {
                Text(name).font(.system(size: 14, weight: .semibold))
                Text(detail).font(.system(size: 12)).foregroundStyle(WatchcatTheme.muted)
            }
            Spacer()
        }
        .padding(.vertical, 11)
    }
}
