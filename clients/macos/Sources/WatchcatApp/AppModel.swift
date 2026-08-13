import Foundation
import ServiceManagement
import Darwin

@_silgen_name("flock")
private func watchcatFlock(_ descriptor: Int32, _ operation: Int32) -> Int32

@MainActor
final class AppModel: ObservableObject {
    static let shared = AppModel()
    @Published private(set) var snapshot = Snapshot.offline
    @Published private(set) var watchlist: [WatchTarget] = []
    @Published private(set) var sessions: [SessionEnvelope] = []
    @Published private(set) var sessionsHasMore = false
    @Published private(set) var policies: [ResolvedPolicy] = []
    @Published private(set) var activity: [SessionLog] = []
    @Published private(set) var isActivityLoading = false
    @Published var selectedSessionID: String?
    @Published var selectedPolicyID: String?
    @Published var search = ""
    @Published var errorMessage: String?
    @Published var lifecycle = LifecycleSettings(
        staleAfterSeconds: 259_200,
        sweepIntervalSeconds: 60,
        protectUnresolvedFailures: true
    )
    @Published private(set) var serviceStatus = "未注册"
    @Published private(set) var commandLineStatus = "尚未同步"
    @Published var mainSection = MainSection.watchlist

    private var refreshTask: Task<Void, Never>?
    private var sessionTitleRefreshTask: Task<Void, Never>?
    private var sessionTitleCache: [String: String] = [:]
    private var sessionSearchCursor: String?
    private var lastSessionTitleRefresh = Date.distantPast
    private let subscriber = EventSubscriber()

    init() {
        refreshTask = Task { [weak self] in
            await self?.refreshAll()
            while !Task.isCancelled {
                try? await Task.sleep(for: .seconds(5))
                await self?.refreshOverview()
            }
        }
        refreshServiceStatus()
        refreshCommandLineStatus()
        subscriber.start { [weak self] in
            await self?.refreshAll()
        }
    }

    deinit {
        refreshTask?.cancel()
        sessionTitleRefreshTask?.cancel()
        subscriber.stop()
    }

    func refreshAll() async {
        await refreshOverviewCore()
        await refreshPolicies()
        await refreshConfig()
    }

    func refreshOverview() async {
        let wasOnline = snapshot.serviceOnline
        await refreshOverviewCore()
        if RefreshPlanner.shouldHydrateDetails(
            wasOnline: wasOnline,
            isOnline: snapshot.serviceOnline,
            hasPolicies: !policies.isEmpty
        ) {
            await refreshPolicies()
            await refreshConfig()
        }
    }

    private func refreshOverviewCore() async {
        await refreshSnapshot()
        await refreshWatchlist()
    }

    func refreshSnapshot() async {
        do {
            let (value, _) = try await RPCClient.shared.call(
                "snapshot.get",
                params: EmptyParams(),
                as: Snapshot.self
            )
            snapshot = value
            errorMessage = nil
        } catch {
            snapshot = .offline
            errorMessage = error.localizedDescription
        }
    }

    func refreshWatchlist() async {
        await perform {
            let (targets, _) = try await RPCClient.shared.call(
                "watch.list",
                params: EmptyParams(),
                as: [WatchTarget].self
            )
            watchlist = titledTargets(targets)
            if targets.contains(where: { $0.label?.isEmpty != false }),
               Date.now.timeIntervalSince(lastSessionTitleRefresh) >= 300,
               sessionTitleRefreshTask == nil
            {
                sessionTitleRefreshTask = Task { [weak self] in
                    await self?.refreshSessionTitles(for: targets)
                }
            }
        }
    }

    private func refreshSessionTitles(for targets: [WatchTarget]) async {
        defer { sessionTitleRefreshTask = nil }
        var resolvedAProvider = false
        for provider in Set(targets.map(\.provider)) {
            guard !Task.isCancelled else { return }
            if let (page, _) = try? await RPCClient.shared.call(
                "sessions.list",
                params: SessionListParams(provider: provider, limit: 500, query: "", cursor: nil),
                as: SessionPage.self
            ) {
                resolvedAProvider = true
                for envelope in page.items where !envelope.session.title.isEmpty {
                    sessionTitleCache["\(provider):\(envelope.session.id)"] = envelope.session.title
                }
            }
        }
        if resolvedAProvider {
            lastSessionTitleRefresh = .now
            watchlist = titledTargets(watchlist)
        }
    }

    private func titledTargets(_ targets: [WatchTarget]) -> [WatchTarget] {
        targets.map { target in
            guard target.label?.isEmpty != false,
                  let title = sessionTitleCache[target.id]
            else { return target }
            var titled = target
            titled.label = title
            return titled
        }
    }

    func searchSessions(reset: Bool) async {
        await perform {
            let (page, _) = try await RPCClient.shared.call(
                "sessions.list",
                params: SessionListParams(
                    provider: "codex",
                    limit: 100,
                    query: search,
                    cursor: reset ? nil : sessionSearchCursor
                ),
                as: SessionPage.self
            )
            sessions = reset ? page.items : sessions + page.items
            sessionSearchCursor = page.nextCursor
            sessionsHasMore = page.hasMore
        }
    }

    func refreshPolicies() async {
        await perform {
            let (value, _) = try await RPCClient.shared.call(
                "policies.list",
                params: EmptyParams(),
                as: [ResolvedPolicy].self
            )
            policies = value
        }
    }

    func refreshConfig() async {
        await perform {
            let (value, _) = try await RPCClient.shared.call(
                "config.get",
                params: EmptyParams(),
                as: ServerSettings.self
            )
            lifecycle = value.lifecycle
        }
    }

    func setGuard(_ enabled: Bool) async {
        struct Params: Codable, Sendable { let enabled: Bool }
        await mutate("guard.set", params: Params(enabled: enabled))
    }

    func pauseGuard() async {
        struct Params: Codable, Sendable { let seconds: Int }
        await mutate("guard.pause", params: Params(seconds: 1_800))
    }

    func retryNow(_ target: WatchTarget) async {
        let requestKey = UUID().uuidString
        await perform {
            let request = RetryRequest(
                provider: target.provider,
                sessionID: target.sessionID,
                requestKey: requestKey
            )
            let operation: RetryOperation
            do {
                (operation, _) = try await RPCClient.shared.call(
                    "sessions.retry_now",
                    params: request,
                    expectedRevision: snapshot.revision,
                    requestID: requestKey,
                    as: RetryOperation.self
                )
            } catch let error as RPCClientError {
                switch error {
                case .socket, .invalidResponse:
                    // A lost local acknowledgement is safe to replay: requestKey
                    // returns the durable operation accepted by the first call.
                    (operation, _) = try await RPCClient.shared.call(
                        "sessions.retry_now",
                        params: request,
                        expectedRevision: snapshot.revision,
                        requestID: requestKey,
                        as: RetryOperation.self
                    )
                default:
                    throw error
                }
            }
            try await awaitRetryOperation(operation.operationID)
        }
    }

    private func awaitRetryOperation(_ operationID: String) async throws {
        while !Task.isCancelled {
            let operation: RetryOperation
            do {
                (operation, _) = try await RPCClient.shared.call(
                    "sessions.retry_status",
                    params: RetryStatusParams(operationID: operationID),
                    as: RetryOperation.self
                )
            } catch {
                throw RPCClientError.server(
                    "立即重试已受理，但暂时无法确认结果。请稍后在活动中查看。\n\(error.localizedDescription)"
                )
            }
            switch operation.status {
            case "succeeded":
                await refreshAll()
                return
            case "failed":
                throw RPCClientError.server(operation.error ?? "立即重试失败。")
            case "unknown":
                throw RPCClientError.server(
                    "恢复请求可能已经发送，但 Watchcat 无法确认最终结果。请查看活动记录。"
                )
            default:
                try await Task.sleep(for: .seconds(1))
            }
        }
        throw CancellationError()
    }

    func interrupt(_ target: WatchTarget) async {
        await perform {
            _ = try await RPCClient.shared.call(
                "sessions.interrupt",
                params: SessionReference(provider: target.provider, sessionID: target.sessionID),
                as: EmptyResult.self
            )
            await loadActivity(sessionID: target.sessionID)
        }
    }

    func toggle(_ target: WatchTarget) async {
        await mutate(
            "watch.update",
            params: WatchUpdateParams(
                provider: target.provider,
                sessionID: target.sessionID,
                enabled: !target.enabled,
                protected: nil
            )
        )
    }

    func setProtected(_ target: WatchTarget, protected: Bool) async {
        await mutate(
            "watch.update",
            params: WatchUpdateParams(
                provider: target.provider,
                sessionID: target.sessionID,
                enabled: nil,
                protected: protected
            )
        )
    }

    func remove(_ target: WatchTarget) async {
        await mutate(
            "watch.remove",
            params: SessionReference(provider: target.provider, sessionID: target.sessionID)
        )
    }

    func add(_ session: AgentSession) async {
        await mutate(
            "watch.add",
            params: WatchAddParams(
                provider: session.provider,
                sessionID: session.id,
                label: session.title,
                protected: false,
                validate: true
            )
        )
        await searchSessions(reset: true)
    }

    func savePolicy(_ policy: ResolvedPolicy) async -> Bool {
        await mutateReportingResult(
            "policies.set",
            params: PolicySetParams(
                condition: policy.condition,
                policy: PolicyOverride(
                    action: policy.action,
                    backoff: policy.action == .retry ? policy.backoff : nil,
                    initialDelaySeconds: policy.action == .retry ? policy.initialDelaySeconds : nil,
                    maxDelaySeconds: policy.action == .retry ? policy.maxDelaySeconds : nil,
                    maxAttempts: policy.action == .retry ? policy.maxAttempts : nil,
                    prompt: policy.action == .retry ? policy.prompt : nil
                )
            )
        )
    }

    func resetPolicy(_ policy: ResolvedPolicy) async -> Bool {
        await mutateReportingResult(
            "policies.reset",
            params: PolicyResetParams(condition: policy.condition)
        )
    }

    func saveLifecycle() async {
        await mutate("config.set_lifecycle", params: lifecycle)
    }

    func loadActivity(sessionID: String) async {
        selectedSessionID = sessionID
        activity = []
        isActivityLoading = true
        defer { isActivityLoading = false }
        do {
            let (value, _) = try await RPCClient.shared.call(
                "sessions.logs",
                params: SessionLogsParams(
                    provider: "codex",
                    sessionID: sessionID,
                    limit: 100,
                    category: nil,
                    reliabilityOnly: true
                ),
                as: [SessionLog].self
            )
            activity = ActivityTimeline.entries(from: value)
            errorMessage = nil
        } catch {
            errorMessage = error.localizedDescription
        }
    }

    func registerService() {
        do {
            try installBundledTools()
            try SMAppService.agent(plistName: "ai.watchcat.watchcatd.plist").register()
            refreshServiceStatus()
        } catch {
            errorMessage = error.localizedDescription
        }
    }

    func syncCommandLineTools() {
        do {
            try installBundledTools()
            errorMessage = nil
        } catch {
            errorMessage = error.localizedDescription
        }
    }

    func unregisterService() {
        do {
            try SMAppService.agent(plistName: "ai.watchcat.watchcatd.plist").unregister()
            refreshServiceStatus()
        } catch {
            errorMessage = error.localizedDescription
        }
    }

    func openLoginItemsSettings() {
        SMAppService.openSystemSettingsLoginItems()
    }

    private func refreshServiceStatus() {
        switch SMAppService.agent(plistName: "ai.watchcat.watchcatd.plist").status {
        case .enabled: serviceStatus = "已启用"
        case .requiresApproval: serviceStatus = "等待系统批准"
        case .notRegistered: serviceStatus = "未注册"
        case .notFound: serviceStatus = "未找到守护服务"
        @unknown default: serviceStatus = "状态未知"
        }
    }

    private var commandLineDirectory: URL {
        FileManager.default.homeDirectoryForCurrentUser
            .appendingPathComponent(".local/bin", isDirectory: true)
    }

    private var bundledVersion: String {
        Bundle.main.object(forInfoDictionaryKey: "CFBundleShortVersionString") as? String ?? "0.4.0"
    }

    private func refreshCommandLineStatus() {
        let fileManager = FileManager.default
        let marker = commandLineDirectory.appendingPathComponent(".watchcat-version")
        let installed = try? String(contentsOf: marker, encoding: .utf8)
            .trimmingCharacters(in: .whitespacesAndNewlines)
        let toolsReady = ["watchcat", "watchcatd"].allSatisfy { name in
            fileManager.isExecutableFile(
                atPath: commandLineDirectory.appendingPathComponent(name).path
            )
        }
        guard installed == bundledVersion && toolsReady else {
            commandLineStatus = "需要同步 · ~/.local/bin"
            return
        }
        let installedTool = commandLineDirectory.appendingPathComponent("watchcat")
            .standardizedFileURL.path
        if let activeTool = executableOnCurrentPath("watchcat") {
            commandLineStatus = activeTool == installedTool
                ? "已同步 \(bundledVersion) · PATH 已生效"
                : "已同步 \(bundledVersion) · PATH 当前指向 \(activeTool)"
        } else {
            commandLineStatus = "已同步 \(bundledVersion) · 请将 ~/.local/bin 加入 PATH"
        }
    }

    private func executableOnCurrentPath(_ name: String) -> String? {
        guard let path = ProcessInfo.processInfo.environment["PATH"] else { return nil }
        let fileManager = FileManager.default
        for directory in path.split(separator: ":", omittingEmptySubsequences: false) {
            let base = directory.isEmpty ? fileManager.currentDirectoryPath : String(directory)
            let candidate = URL(fileURLWithPath: base, isDirectory: true)
                .appendingPathComponent(name)
                .standardizedFileURL.path
            if fileManager.isExecutableFile(atPath: candidate) {
                return candidate
            }
        }
        return nil
    }

    private func installBundledTools() throws {
        let fileManager = FileManager.default
        let serviceLock = try acquireServiceLockForToolSync(fileManager)
        defer {
            _ = watchcatFlock(serviceLock, LOCK_UN)
            Darwin.close(serviceLock)
        }
        guard let resources = Bundle.main.resourceURL else {
            throw RPCClientError.socket("App 内缺少命令行工具资源。")
        }
        try fileManager.createDirectory(
            at: commandLineDirectory,
            withIntermediateDirectories: true,
            attributes: [.posixPermissions: 0o755]
        )
        let transaction = UUID().uuidString
        var staged: [(temporary: URL, destination: URL, backup: URL?)] = []
        var committed = 0
        do {
            for name in ["watchcat", "watchcatd"] {
                let source = resources.appendingPathComponent(name)
                guard fileManager.isExecutableFile(atPath: source.path) else {
                    throw RPCClientError.socket("App 内缺少可执行文件：\(name)")
                }
                let temporary = commandLineDirectory.appendingPathComponent(".\(name).\(transaction).tmp")
                let destination = commandLineDirectory.appendingPathComponent(name)
                let backup = commandLineDirectory.appendingPathComponent(".\(name).\(transaction).backup")
                try fileManager.copyItem(at: source, to: temporary)
                try fileManager.setAttributes([.posixPermissions: 0o755], ofItemAtPath: temporary.path)
                if fileManager.fileExists(atPath: destination.path) {
                    try fileManager.copyItem(at: destination, to: backup)
                    staged.append((temporary, destination, backup))
                } else {
                    staged.append((temporary, destination, nil))
                }
            }
            for item in staged {
                if fileManager.fileExists(atPath: item.destination.path) {
                    _ = try fileManager.replaceItemAt(item.destination, withItemAt: item.temporary)
                } else {
                    try fileManager.moveItem(at: item.temporary, to: item.destination)
                }
                committed += 1
            }
            let marker = commandLineDirectory.appendingPathComponent(".watchcat-version")
            try "\(bundledVersion)\n".write(to: marker, atomically: true, encoding: .utf8)
            for item in staged {
                if let backup = item.backup {
                    try? fileManager.removeItem(at: backup)
                }
            }
            refreshCommandLineStatus()
        } catch {
            for item in staged.prefix(committed).reversed() {
                try? fileManager.removeItem(at: item.destination)
                if let backup = item.backup {
                    try? fileManager.moveItem(at: backup, to: item.destination)
                }
            }
            for item in staged {
                for artifact in [item.temporary, item.backup].compactMap({ $0 })
                    where fileManager.fileExists(atPath: artifact.path)
                {
                    try? fileManager.removeItem(at: artifact)
                }
            }
            throw error
        }
    }

    private func acquireServiceLockForToolSync(_ fileManager: FileManager) throws -> Int32 {
        let stateDirectory = fileManager.urls(
            for: .applicationSupportDirectory,
            in: .userDomainMask
        ).first!.appendingPathComponent("ai.watchcat.watchcat", isDirectory: true)
        try fileManager.createDirectory(
            at: stateDirectory,
            withIntermediateDirectories: true,
            attributes: [.posixPermissions: 0o700]
        )
        try fileManager.setAttributes(
            [.posixPermissions: 0o700],
            ofItemAtPath: stateDirectory.path
        )
        let lockPath = stateDirectory.appendingPathComponent("watchcat.lock").path
        let descriptor = Darwin.open(lockPath, O_RDWR | O_CREAT, S_IRUSR | S_IWUSR)
        guard descriptor >= 0 else {
            throw RPCClientError.socket("无法检查后台服务状态。")
        }
        guard watchcatFlock(descriptor, LOCK_EX | LOCK_NB) == 0 else {
            Darwin.close(descriptor)
            if errno == EWOULDBLOCK {
                throw RPCClientError.socket("请先停用正在运行的 Watchcat 服务，再同步命令行工具。")
            }
            throw RPCClientError.socket("无法检查后台服务状态。")
        }
        return descriptor
    }

    private func mutate<Params: Encodable & Sendable>(_ method: String, params: Params) async {
        _ = await mutateReportingResult(method, params: params)
    }

    private func mutateReportingResult<Params: Encodable & Sendable>(
        _ method: String,
        params: Params
    ) async -> Bool {
        do {
            _ = try await RPCClient.shared.call(
                method,
                params: params,
                expectedRevision: snapshot.revision,
                as: EmptyResult.self
            )
            await refreshAll()
            errorMessage = nil
            return true
        } catch {
            errorMessage = error.localizedDescription
            return false
        }
    }

    private func perform(_ action: () async throws -> Void) async {
        do {
            try await action()
            errorMessage = nil
        } catch {
            errorMessage = error.localizedDescription
        }
    }
}
