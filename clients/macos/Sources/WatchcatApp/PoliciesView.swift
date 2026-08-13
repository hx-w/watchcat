import SwiftUI

struct PoliciesView: View {
    @EnvironmentObject private var model: AppModel

    private let categoryNames = [
        "network": "网络",
        "capacity": "容量与服务",
        "service": "容量与服务",
        "retry": "容量与服务",
        "auth": "账户与能力",
        "billing": "账户与能力",
        "capability": "账户与能力",
        "context": "上下文、配额与请求",
        "quota": "上下文、配额与请求",
        "request": "上下文、配额与请求",
        "sandbox": "上下文、配额与请求",
        "failure": "上下文、配额与请求",
    ]

    private var groups: [(String, [ResolvedPolicy])] {
        let order = ["网络", "容量与服务", "账户与能力", "上下文、配额与请求"]
        let grouped = Dictionary(grouping: model.policies) { categoryNames[$0.category] ?? $0.category }
        return order.compactMap { name in grouped[name].map { (name, $0) } }
    }

    var body: some View {
        ScrollView {
            LazyVStack(alignment: .leading, spacing: 18) {
                header
                ForEach(groups, id: \.0) { name, policies in
                    VStack(alignment: .leading, spacing: 7) {
                        Text(name).font(.system(size: 12, weight: .semibold)).foregroundStyle(WatchcatTheme.muted)
                        VStack(spacing: 0) {
                            ForEach(policies) { policy in
                                PolicyRow(policy: policy)
                                if policy.id != policies.last?.id { Divider() }
                            }
                        }
                        .background(WatchcatTheme.surface)
                        .clipShape(RoundedRectangle(cornerRadius: 12))
                        .overlay(RoundedRectangle(cornerRadius: 12).stroke(WatchcatTheme.line))
                    }
                }
            }
            .padding(24)
        }
        .background(WatchcatTheme.paper)
    }

    private var header: some View {
        HStack(alignment: .top) {
            VStack(alignment: .leading, spacing: 5) {
                Text("恢复策略").font(.system(size: 26, weight: .bold))
                Text("修改后由 Watchcat 服务校验并立即同步。")
                    .font(.system(size: 13))
                    .foregroundStyle(WatchcatTheme.muted)
            }
            Spacer()
            HStack(spacing: 6) {
                StatusDot(color: WatchcatTheme.green)
                Text("已同步")
                    .font(.system(size: 12, weight: .semibold))
            }
            .padding(.horizontal, 12)
            .frame(height: 34)
            .background(Color.black.opacity(0.045))
            .clipShape(Capsule())
        }
    }
}

private struct PolicyRow: View {
    @EnvironmentObject private var model: AppModel
    @State private var draft: ResolvedPolicy

    init(policy: ResolvedPolicy) {
        _draft = State(initialValue: policy)
    }

    private var expanded: Bool { model.selectedPolicyID == draft.id }

    var body: some View {
        VStack(spacing: 0) {
            HStack(spacing: 14) {
                VStack(alignment: .leading, spacing: 3) {
                    Text(policyTitle(draft)).font(.system(size: 14, weight: .semibold))
                    Text(draft.condition).font(.system(size: 11, design: .monospaced)).foregroundStyle(WatchcatTheme.muted)
                }
                Spacer()
                Text(draft.action == .retry ? "重试" : "跳过")
                    .font(.system(size: 12, weight: .semibold))
                    .foregroundStyle(draft.action == .retry ? WatchcatTheme.ink : WatchcatTheme.muted)
                Button {
                    withAnimation(.easeOut(duration: 0.16)) {
                        model.selectedPolicyID = expanded ? nil : draft.id
                    }
                } label: {
                    Image(systemName: expanded ? "chevron.down" : "chevron.right")
                        .frame(width: 36, height: 36)
                        .background(expanded ? WatchcatTheme.navySoft : .clear)
                        .clipShape(RoundedRectangle(cornerRadius: 8))
                }
                .buttonStyle(.plain)
                .accessibilityLabel("编辑 \(policyTitle(draft))")
            }
            .padding(.horizontal, 16)
            .frame(minHeight: 66)
            .background(expanded ? Color.black.opacity(0.035) : .clear)

            if expanded {
                editor
                    .transition(.opacity.combined(with: .move(edge: .top)))
            }
        }
        .onChange(of: draft.action) { _ in
            if !expanded { model.selectedPolicyID = draft.id }
        }
        .onChange(of: model.policies) { policies in
            if !expanded, let refreshed = policies.first(where: { $0.id == draft.id }) {
                draft = refreshed
            }
        }
    }

    private var editor: some View {
        VStack(alignment: .leading, spacing: 16) {
            HStack(alignment: .top, spacing: 12) {
                policyField("处理动作") {
                    Picker("", selection: $draft.action) {
                        Text("重试").tag(PolicyAction.retry)
                        Text("跳过").tag(PolicyAction.skip)
                    }
                    .labelsHidden()
                    .pickerStyle(.segmented)
                    .frame(maxWidth: .infinity)
                }
                .frame(maxWidth: .infinity)

                policyField("配置状态") {
                    HStack(spacing: 7) {
                        StatusDot(color: draft.customized ? WatchcatTheme.green : WatchcatTheme.muted.opacity(0.55))
                        Text(draft.customized ? "已自定义" : "使用默认值")
                            .font(.system(size: 12, weight: .medium))
                        Spacer()
                    }
                    .padding(.horizontal, 10)
                    .frame(maxWidth: .infinity, minHeight: 28)
                    .background(Color.black.opacity(0.035))
                    .clipShape(RoundedRectangle(cornerRadius: 7))
                }
                .frame(maxWidth: .infinity)
            }
            if draft.action == .retry {
                policyField("退让策略") {
                    Picker("", selection: Binding(
                        get: { draft.backoff ?? .exponential },
                        set: { draft.backoff = $0 }
                    )) {
                        Text("指数退让").tag(BackoffKind.exponential)
                        Text("固定间隔").tag(BackoffKind.fixed)
                    }
                    .labelsHidden()
                    .pickerStyle(.segmented)
                    .frame(maxWidth: 360)
                }

                HStack(alignment: .top, spacing: 12) {
                    policyField("首次等待（秒）") {
                        numberField($draft.initialDelaySeconds)
                    }
                    .frame(maxWidth: .infinity)
                    policyField("最长等待（秒）") {
                        numberField($draft.maxDelaySeconds)
                    }
                    .frame(maxWidth: .infinity)
                    policyField("最多尝试") {
                        TextField("", value: $draft.maxAttempts, format: .number)
                            .textFieldStyle(.roundedBorder)
                            .frame(maxWidth: .infinity, minHeight: 30)
                    }
                    .frame(maxWidth: .infinity)
                }
                policyField("恢复提示词") {
                    TextEditor(text: $draft.prompt)
                        .font(.system(size: 12))
                        .frame(minHeight: 94)
                        .padding(7)
                        .background(WatchcatTheme.surface)
                        .clipShape(RoundedRectangle(cornerRadius: 8))
                        .overlay(RoundedRectangle(cornerRadius: 8).stroke(WatchcatTheme.line))
                    Text("可用变量：{provider} {model} {condition} {provider_code} {attempt} {max_attempts}")
                        .font(.system(size: 10))
                        .foregroundStyle(WatchcatTheme.muted)
                }
            }
            HStack {
                Button("恢复默认") {
                    Task {
                        if await model.resetPolicy(draft) {
                            if let refreshed = model.policies.first(where: { $0.id == draft.id }) {
                                draft = refreshed
                            }
                            model.selectedPolicyID = nil
                        }
                    }
                }
                    .buttonStyle(QuietButtonStyle())
                Spacer()
                Button("应用并同步") {
                    Task {
                        if await model.savePolicy(draft) {
                            if let refreshed = model.policies.first(where: { $0.id == draft.id }) {
                                draft = refreshed
                            }
                            model.selectedPolicyID = nil
                        }
                    }
                }
                    .buttonStyle(QuietButtonStyle(prominent: true))
            }
        }
        .padding(.horizontal, 18)
        .padding(.vertical, 16)
        .background(Color.black.opacity(0.025))
        .overlay(alignment: .top) { Divider() }
    }

    private func policyField<Content: View>(_ title: String, @ViewBuilder content: () -> Content) -> some View {
        VStack(alignment: .leading, spacing: 6) {
            Text(title).font(.system(size: 11, weight: .semibold)).foregroundStyle(WatchcatTheme.muted)
            content()
        }
    }

    private func numberField(_ value: Binding<UInt64>) -> some View {
        TextField("", value: value, format: .number)
            .textFieldStyle(.roundedBorder)
            .frame(maxWidth: .infinity, minHeight: 30)
    }

    private func policyTitle(_ policy: ResolvedPolicy) -> String {
        let titles = [
            "network.connection_failed": "无法建立或维持连接",
            "network.stream_failed": "响应流中断",
            "network.timeout": "请求超时",
            "capacity.model_overloaded": "模型暂时繁忙",
            "capacity.service_overloaded": "服务暂时繁忙",
            "capacity.rate_limited": "请求频率受限",
            "capacity.server_throttled": "服务端限流",
            "service.server_error": "服务端临时错误",
            "service.conflict": "请求发生临时冲突",
            "retry.provider_exhausted": "Provider 已用尽内部重试",
            "auth.invalid": "登录状态无效或已过期",
            "billing.required": "账单或额度需要处理",
            "capability.model_unavailable": "当前账户无法使用该模型",
            "capability.access_denied": "账户无权使用该能力",
            "capability.feature_unsupported": "模型不支持所需功能",
            "capability.entitlement_required": "需要额外权限",
            "capability.verification_required": "账户需要进一步验证",
            "context.window_exceeded": "超过模型上下文窗口",
            "context.output_limit": "达到输出长度上限",
            "quota.usage_exhausted": "账户用量已耗尽",
            "request.invalid": "请求无效",
            "request.too_large": "请求内容过大",
            "sandbox.failed": "本地沙箱执行失败",
            "failure.unknown": "无法安全识别异常",
        ]
        return titles[policy.condition] ?? policy.description
    }
}
