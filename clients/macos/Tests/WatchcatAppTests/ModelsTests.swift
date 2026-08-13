import XCTest
@testable import WatchcatApp

final class ModelsTests: XCTestCase {
    func testSnapshotDecodesSnakeCaseContract() throws {
        let data = Data(#"{"generated_at":"2026-08-12T12:00:00Z","revision":9,"service_online":true,"guard_enabled":true,"guard_paused_until":null,"watched":6,"paused":1,"attention":2,"attention_target_keys":["codex:a","codex:b"],"automatic_recoveries":27,"hands_free_percent":93}"#.utf8)
        let snapshot = try CodingTools.decoder.decode(Snapshot.self, from: data)
        XCTAssertEqual(snapshot.revision, 9)
        XCTAssertEqual(snapshot.automaticRecoveries, 27)
        XCTAssertEqual(snapshot.handsFreePercent, 93)
    }

    func testPolicyRoundTripsWithoutLosingPrompt() throws {
        let policy = PolicyOverride(
            action: .retry,
            backoff: .exponential,
            initialDelaySeconds: 15,
            maxDelaySeconds: 300,
            maxAttempts: 5,
            prompt: "Continue {condition}"
        )
        let data = try CodingTools.encoder.encode(policy)
        let decoded = try CodingTools.decoder.decode(PolicyOverride.self, from: data)
        XCTAssertEqual(decoded.prompt, policy.prompt)
        XCTAssertEqual(decoded.maxAttempts, 5)
    }

    func testWatchTargetDecodesFractionalServerTimestamp() throws {
        let data = Data(#"[{"provider":"codex","session_id":"one","enabled":true,"protected":false,"label":"One","added_at":"2026-08-12T15:18:16.625644Z","last_event_at":null}]"#.utf8)
        let targets = try CodingTools.decoder.decode([WatchTarget].self, from: data)
        XCTAssertEqual(targets.first?.sessionID, "one")
    }

    func testWatchlistPagingShowsEverySessionInPagesOfFive() {
        let sessions = Array(0..<12)

        XCTAssertEqual(WatchlistPaging.pageCount(itemCount: sessions.count), 3)
        XCTAssertEqual(WatchlistPaging.items(sessions, page: 0), [0, 1, 2, 3, 4])
        XCTAssertEqual(WatchlistPaging.items(sessions, page: 1), [5, 6, 7, 8, 9])
        XCTAssertEqual(WatchlistPaging.items(sessions, page: 2), [10, 11])
    }

    func testWatchlistPagingClampsAfterLastPageIsRemoved() {
        XCTAssertEqual(WatchlistPaging.clampedPage(2, itemCount: 6), 1)
        XCTAssertEqual(WatchlistPaging.items(Array(0..<6), page: 2), [5])
        XCTAssertEqual(WatchlistPaging.items([Int](), page: 4), [])
    }

    func testReconnectHydratesPoliciesThatMissedInitialLoad() {
        XCTAssertTrue(
            RefreshPlanner.shouldHydrateDetails(
                wasOnline: false,
                isOnline: true,
                hasPolicies: false
            )
        )
        XCTAssertFalse(
            RefreshPlanner.shouldHydrateDetails(
                wasOnline: true,
                isOnline: true,
                hasPolicies: true
            )
        )
    }

    func testActivityTimelineKeepsReliabilityEventsAndDeduplicatesWaiting() {
        let now = Date(timeIntervalSince1970: 100)
        let logs = [
            SessionLog(timestamp: now, source: "provider", kind: "message", role: "assistant", turnID: "turn-1", condition: nil, message: "ordinary reply"),
            SessionLog(timestamp: now, source: "provider", kind: "turn.completed", role: nil, turnID: "turn-1", condition: nil, message: "turn completed"),
            SessionLog(timestamp: now, source: "provider", kind: "turn.failed", role: nil, turnID: "turn-2", condition: "network.timeout", message: "request timed out"),
            SessionLog(timestamp: now, source: "watchcat", kind: "retry.waiting", role: nil, turnID: "turn-2", condition: "network.timeout", message: "first poll"),
            SessionLog(timestamp: now.addingTimeInterval(5), source: "watchcat", kind: "retry.waiting", role: nil, turnID: "turn-2", condition: "network.timeout", message: "second poll"),
            SessionLog(timestamp: now.addingTimeInterval(10), source: "watchcat", kind: "retry.sent", role: nil, turnID: "turn-2", condition: "network.timeout", message: "resume sent"),
        ]

        let timeline = ActivityTimeline.entries(from: logs)

        XCTAssertEqual(timeline.map(\.kind), ["turn.failed", "retry.waiting", "retry.sent"])
        XCTAssertEqual(timeline[1].message, "second poll")
    }

    func testRejectsIncompatibleServiceProtocol() {
        XCTAssertThrowsError(try validateProtocolVersion(watchcatProtocolVersion + 1)) { error in
            guard case RPCClientError.incompatibleProtocol = error else {
                return XCTFail("unexpected error: \(error)")
            }
        }
    }
}
