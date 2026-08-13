import Foundation

struct Snapshot: Codable, Equatable, Sendable {
    let generatedAt: Date
    let revision: UInt64
    let serviceOnline: Bool
    let guardEnabled: Bool
    let guardPausedUntil: Date?
    let watched: Int
    let paused: Int
    let attention: Int
    let attentionTargetKeys: [String]
    let automaticRecoveries: Int
    let handsFreePercent: Int

    static let offline = Snapshot(
        generatedAt: .now,
        revision: 0,
        serviceOnline: false,
        guardEnabled: false,
        guardPausedUntil: nil,
        watched: 0,
        paused: 0,
        attention: 0,
        attentionTargetKeys: [],
        automaticRecoveries: 0,
        handsFreePercent: 100
    )
}

struct WatchTarget: Codable, Identifiable, Equatable, Sendable {
    let provider: String
    let sessionID: String
    var enabled: Bool
    var protected: Bool
    var label: String?
    let addedAt: Date
    var lastEventAt: Date?

    var id: String { "\(provider):\(sessionID)" }
    var displayName: String {
        if let label, !label.isEmpty { return label }
        let providerName = provider == "codex" ? "Codex" : provider.capitalized
        return "\(providerName) 会话 · \(sessionID.prefix(8))"
    }

    private enum CodingKeys: String, CodingKey {
        case provider, enabled, protected, label
        case sessionID = "sessionId"
        case addedAt, lastEventAt
    }
}

enum WatchlistPaging {
    static let pageSize = 5

    static func pageCount(itemCount: Int) -> Int {
        max(1, (itemCount + pageSize - 1) / pageSize)
    }

    static func clampedPage(_ page: Int, itemCount: Int) -> Int {
        min(max(0, page), pageCount(itemCount: itemCount) - 1)
    }

    static func items<Element>(_ items: [Element], page: Int) -> [Element] {
        let safePage = clampedPage(page, itemCount: items.count)
        let start = safePage * pageSize
        let end = min(start + pageSize, items.count)
        guard start < end else { return [] }
        return Array(items[start..<end])
    }
}

enum RefreshPlanner {
    static func shouldHydrateDetails(
        wasOnline: Bool,
        isOnline: Bool,
        hasPolicies: Bool
    ) -> Bool {
        isOnline && (!wasOnline || !hasPolicies)
    }
}

enum SessionState: String, Codable, Sendable {
    case unknown, idle, active, failed
}

struct AgentSession: Codable, Identifiable, Equatable, Sendable {
    let provider: String
    let id: String
    let title: String
    let state: SessionState
    let updatedAt: Date?
}

struct SessionEnvelope: Codable, Identifiable, Equatable, Sendable {
    let watched: Bool
    let session: AgentSession
    var id: String { session.id }
}

struct SessionPage: Codable, Sendable {
    let items: [SessionEnvelope]
    let nextCursor: String?
    let hasMore: Bool
}

enum PolicyAction: String, Codable, CaseIterable, Sendable {
    case retry, skip
}

enum BackoffKind: String, Codable, CaseIterable, Sendable {
    case fixed, exponential
}

struct ResolvedPolicy: Codable, Identifiable, Equatable, Sendable {
    let condition: String
    let description: String
    var action: PolicyAction
    var backoff: BackoffKind?
    var initialDelaySeconds: UInt64
    var maxDelaySeconds: UInt64
    var maxAttempts: Int
    var prompt: String
    var customized: Bool
    var id: String { condition }
    var category: String { condition.split(separator: ".").first.map(String.init) ?? condition }
}

struct PolicyOverride: Codable, Sendable {
    let action: PolicyAction?
    let backoff: BackoffKind?
    let initialDelaySeconds: UInt64?
    let maxDelaySeconds: UInt64?
    let maxAttempts: Int?
    let prompt: String?
}

struct SessionLog: Codable, Identifiable, Sendable {
    let timestamp: Date?
    let source: String
    let kind: String
    let role: String?
    let turnID: String?
    let condition: String?
    let message: String
    var id: String { "\(timestamp?.timeIntervalSince1970 ?? 0):\(kind):\(turnID ?? message)" }

    private enum CodingKeys: String, CodingKey {
        case timestamp, source, kind, role, condition, message
        case turnID = "turnId"
    }
}

enum ActivityTimeline {
    static func entries(from logs: [SessionLog]) -> [SessionLog] {
        var entries: [SessionLog] = []
        var repeatedEventIndexes: [RepeatedEvent: Int] = [:]

        for log in logs where isReliabilityEvent(log) {
            guard let repeatedEvent = RepeatedEvent(log) else {
                entries.append(log)
                continue
            }
            if let index = repeatedEventIndexes[repeatedEvent] {
                entries[index] = log
            } else {
                repeatedEventIndexes[repeatedEvent] = entries.count
                entries.append(log)
            }
        }

        return entries.sorted { left, right in
            (left.timestamp ?? .distantPast) < (right.timestamp ?? .distantPast)
        }
    }

    private static func isReliabilityEvent(_ log: SessionLog) -> Bool {
        if log.source == "watchcat" { return true }
        if log.condition != nil { return true }
        return log.kind == "turn.failed" || log.kind == "provider.error"
    }

    private struct RepeatedEvent: Hashable {
        let source: String
        let kind: String
        let turnID: String
        let condition: String?

        init?(_ log: SessionLog) {
            guard let turnID = log.turnID else { return nil }
            source = log.source
            kind = log.kind
            self.turnID = turnID
            condition = log.condition
        }
    }
}

struct LifecycleSettings: Codable, Equatable, Sendable {
    var staleAfterSeconds: Int
    var sweepIntervalSeconds: UInt64
    var protectUnresolvedFailures: Bool
}

struct ServerSettings: Codable, Sendable {
    let lifecycle: LifecycleSettings
}

struct EmptyParams: Codable, Sendable {}

struct SessionReference: Codable, Sendable {
    let provider: String
    let sessionID: String
}

struct RetryRequest: Codable, Sendable {
    let provider: String
    let sessionID: String
    let requestKey: String
}

struct RetryOperation: Codable, Sendable {
    let operationID: String
    let provider: String
    let sessionID: String
    let status: String
    let error: String?
}

struct RetryStatusParams: Codable, Sendable {
    let operationID: String
}

struct SessionListParams: Codable, Sendable {
    let provider: String
    let limit: Int
    let query: String
    let cursor: String?
}

struct SessionLogsParams: Codable, Sendable {
    let provider: String
    let sessionID: String
    let limit: Int
    let category: String?
    let reliabilityOnly: Bool
}

struct WatchAddParams: Codable, Sendable {
    let provider: String
    let sessionID: String
    let label: String?
    let protected: Bool
    let validate: Bool
}

struct WatchUpdateParams: Codable, Sendable {
    let provider: String
    let sessionID: String
    let enabled: Bool?
    let protected: Bool?
}

struct PolicySetParams: Codable, Sendable {
    let condition: String
    let policy: PolicyOverride
}

struct PolicyResetParams: Codable, Sendable {
    let condition: String?
}

enum CodingTools {
    static let encoder: JSONEncoder = {
        let encoder = JSONEncoder()
        encoder.keyEncodingStrategy = .convertToSnakeCase
        encoder.dateEncodingStrategy = .iso8601
        return encoder
    }()

    static let decoder: JSONDecoder = {
        let decoder = JSONDecoder()
        decoder.keyDecodingStrategy = .convertFromSnakeCase
        decoder.dateDecodingStrategy = .iso8601
        return decoder
    }()
}
