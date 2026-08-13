import Foundation
import Darwin

struct RPCNotificationEnvelope: Decodable, Sendable {
    let version: UInt32
    let event: String
    let revision: UInt64
}

final class EventSubscriber: @unchecked Sendable {
    private var task: Task<Void, Never>?
    private let maxFrameBytes = 8 * 1024 * 1024
    private let timeoutSeconds = 35

    func start(onEvent: @escaping @MainActor @Sendable () async -> Void) {
        task?.cancel()
        task = Task.detached { [weak self] in
            guard let self else { return }
            while !Task.isCancelled {
                do {
                    try self.subscribe(onEvent: onEvent)
                } catch {
                    try? await Task.sleep(for: .seconds(2))
                }
            }
        }
    }

    func stop() { task?.cancel() }

    private func subscribe(onEvent: @escaping @MainActor @Sendable () async -> Void) throws {
        let fd = try connectSocket()
        defer { Darwin.close(fd) }
        let request = RPCRequest(
            id: UUID().uuidString,
            method: "events.subscribe",
            params: EmptyParams(),
            expectedRevision: nil
        )
        try writeFrame(CodingTools.encoder.encode(request), to: fd)
        let response = try CodingTools.decoder.decode(
            RPCResponse<EmptyResult>.self,
            from: readFrame(from: fd)
        )
        try validateProtocolVersion(response.version)
        if let error = response.error {
            throw RPCClientError.server("\(error.code): \(error.message)")
        }
        guard response.id == request.id, response.result != nil else {
            throw RPCClientError.invalidResponse
        }
        Task { @MainActor in await onEvent() }
        while !Task.isCancelled {
            let data = try readFrame(from: fd)
            let notification = try CodingTools.decoder.decode(RPCNotificationEnvelope.self, from: data)
            try validateProtocolVersion(notification.version)
            if notification.event != "service.heartbeat" {
                Task { @MainActor in await onEvent() }
            }
        }
    }

    private func connectSocket() throws -> Int32 {
        let path: String
        if let state = ProcessInfo.processInfo.environment["WATCHCAT_STATE_DIR"], !state.isEmpty {
            path = URL(fileURLWithPath: state).appendingPathComponent("watchcat.sock").path
        } else {
            path = FileManager.default.urls(for: .applicationSupportDirectory, in: .userDomainMask).first!
                .appendingPathComponent("ai.watchcat.watchcat/watchcat.sock").path
        }
        let fd = Darwin.socket(AF_UNIX, SOCK_STREAM, 0)
        guard fd >= 0 else { throw RPCClientError.socket("无法创建事件连接。") }
        var timeout = timeval(tv_sec: timeoutSeconds, tv_usec: 0)
        guard setsockopt(fd, SOL_SOCKET, SO_RCVTIMEO, &timeout, socklen_t(MemoryLayout<timeval>.size)) == 0,
              setsockopt(fd, SOL_SOCKET, SO_SNDTIMEO, &timeout, socklen_t(MemoryLayout<timeval>.size)) == 0
        else {
            Darwin.close(fd)
            throw RPCClientError.socket("无法设置事件连接超时。")
        }
        var address = sockaddr_un()
        address.sun_family = sa_family_t(AF_UNIX)
        let maximum = MemoryLayout.size(ofValue: address.sun_path)
        guard path.utf8.count < maximum else { throw RPCClientError.socket("事件 socket 路径过长。") }
        withUnsafeMutablePointer(to: &address.sun_path) { pointer in
            pointer.withMemoryRebound(to: CChar.self, capacity: maximum) { destination in
                _ = path.withCString { source in strcpy(destination, source) }
            }
        }
        let size = socklen_t(MemoryLayout<sa_family_t>.size + path.utf8.count + 1)
        let result = withUnsafePointer(to: &address) { pointer in
            pointer.withMemoryRebound(to: sockaddr.self, capacity: 1) { Darwin.connect(fd, $0, size) }
        }
        guard result == 0 else { Darwin.close(fd); throw RPCClientError.socket("事件连接失败。") }
        return fd
    }

    private func writeFrame(_ data: Data, to fd: Int32) throws {
        var length = UInt32(data.count).bigEndian
        try withUnsafeBytes(of: &length) { try writeAll($0, to: fd) }
        try data.withUnsafeBytes { try writeAll($0, to: fd) }
    }

    private func readFrame(from fd: Int32) throws -> Data {
        var length = UInt32.zero
        try withUnsafeMutableBytes(of: &length) { try readAll($0, from: fd) }
        let count = Int(UInt32(bigEndian: length))
        guard count <= maxFrameBytes else { throw RPCClientError.oversizedFrame }
        var data = Data(count: count)
        try data.withUnsafeMutableBytes { try readAll($0, from: fd) }
        return data
    }

    private func writeAll(_ bytes: UnsafeRawBufferPointer, to fd: Int32) throws {
        var offset = 0
        while offset < bytes.count {
            let count = Darwin.write(fd, bytes.baseAddress!.advanced(by: offset), bytes.count - offset)
            guard count > 0 else { throw RPCClientError.socket("事件连接写入失败。") }
            offset += count
        }
    }

    private func readAll(_ bytes: UnsafeMutableRawBufferPointer, from fd: Int32) throws {
        var offset = 0
        while offset < bytes.count {
            let count = Darwin.read(fd, bytes.baseAddress!.advanced(by: offset), bytes.count - offset)
            guard count > 0 else { throw RPCClientError.socket("事件连接已断开。") }
            offset += count
        }
    }
}
