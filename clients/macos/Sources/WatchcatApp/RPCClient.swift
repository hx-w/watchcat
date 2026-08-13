import Foundation
import Darwin

let watchcatProtocolVersion: UInt32 = 1

struct RPCErrorPayload: Codable {
    let code: String
    let message: String
}

struct RPCResponse<Result: Decodable>: Decodable {
    let version: UInt32
    let id: String
    let revision: UInt64
    let result: Result?
    let error: RPCErrorPayload?
}

struct RPCRequest<Params: Encodable>: Encodable {
    let version: UInt32 = watchcatProtocolVersion
    let id: String
    let method: String
    let params: Params
    let expectedRevision: UInt64?
}

struct EmptyResult: Codable, Sendable {}

enum RPCClientError: LocalizedError {
    case socket(String)
    case server(String)
    case oversizedFrame
    case invalidResponse
    case incompatibleProtocol(UInt32)

    var errorDescription: String? {
        switch self {
        case .socket(let detail): detail
        case .server(let detail): detail
        case .oversizedFrame: "Watchcat 返回了过大的数据帧。"
        case .invalidResponse: "Watchcat 返回了无法解析的数据。"
        case .incompatibleProtocol(let version):
            "Watchcat 服务协议版本 \(version) 不受支持，请同时更新客户端和服务。"
        }
    }
}

actor RPCClient {
    static let shared = RPCClient()
    private let maxFrameBytes = 8 * 1024 * 1024
    private let timeoutSeconds = 35

    func call<Params: Encodable & Sendable, Result: Decodable & Sendable>(
        _ method: String,
        params: Params,
        expectedRevision: UInt64? = nil,
        requestID: String = UUID().uuidString,
        as resultType: Result.Type = Result.self
    ) throws -> (Result, UInt64) {
        let request = RPCRequest(
            id: requestID,
            method: method,
            params: params,
            expectedRevision: expectedRevision
        )
        let payload = try CodingTools.encoder.encode(request)
        let fd = try connectSocket()
        defer { Darwin.close(fd) }
        try writeFrame(payload, to: fd)
        let responseData = try readFrame(from: fd)
        let response = try CodingTools.decoder.decode(RPCResponse<Result>.self, from: responseData)
        try validateProtocolVersion(response.version)
        if let error = response.error {
            throw RPCClientError.server("\(error.code): \(error.message)")
        }
        guard response.id == request.id, let result = response.result else {
            throw RPCClientError.invalidResponse
        }
        return (result, response.revision)
    }

    private func connectSocket() throws -> Int32 {
        let path = socketPath()
        let fd = Darwin.socket(AF_UNIX, SOCK_STREAM, 0)
        guard fd >= 0 else { throw RPCClientError.socket("无法创建本地连接。") }
        var timeout = timeval(tv_sec: timeoutSeconds, tv_usec: 0)
        guard setsockopt(fd, SOL_SOCKET, SO_RCVTIMEO, &timeout, socklen_t(MemoryLayout<timeval>.size)) == 0,
              setsockopt(fd, SOL_SOCKET, SO_SNDTIMEO, &timeout, socklen_t(MemoryLayout<timeval>.size)) == 0
        else {
            Darwin.close(fd)
            throw RPCClientError.socket("无法设置本地连接超时。")
        }
        var address = sockaddr_un()
        address.sun_family = sa_family_t(AF_UNIX)
        let maximum = MemoryLayout.size(ofValue: address.sun_path)
        guard path.utf8.count < maximum else {
            Darwin.close(fd)
            throw RPCClientError.socket("Watchcat socket 路径过长。")
        }
        withUnsafeMutablePointer(to: &address.sun_path) { pointer in
            pointer.withMemoryRebound(to: CChar.self, capacity: maximum) { destination in
                _ = path.withCString { source in strcpy(destination, source) }
            }
        }
        let size = socklen_t(MemoryLayout<sa_family_t>.size + path.utf8.count + 1)
        let result = withUnsafePointer(to: &address) { pointer in
            pointer.withMemoryRebound(to: sockaddr.self, capacity: 1) {
                Darwin.connect(fd, $0, size)
            }
        }
        guard result == 0 else {
            let detail = String(cString: strerror(errno))
            Darwin.close(fd)
            throw RPCClientError.socket("无法连接 Watchcat 服务：\(detail)")
        }
        return fd
    }

    private func socketPath() -> String {
        if let state = ProcessInfo.processInfo.environment["WATCHCAT_STATE_DIR"], !state.isEmpty {
            return URL(fileURLWithPath: state).appendingPathComponent("watchcat.sock").path
        }
        let base = FileManager.default.urls(for: .applicationSupportDirectory, in: .userDomainMask).first!
        return base
            .appendingPathComponent("ai.watchcat.watchcat")
            .appendingPathComponent("watchcat.sock")
            .path
    }

    private func writeFrame(_ data: Data, to fd: Int32) throws {
        guard data.count <= maxFrameBytes else { throw RPCClientError.oversizedFrame }
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
            let written = Darwin.write(fd, bytes.baseAddress!.advanced(by: offset), bytes.count - offset)
            guard written > 0 else { throw RPCClientError.socket("本地连接写入失败。") }
            offset += written
        }
    }

    private func readAll(_ bytes: UnsafeMutableRawBufferPointer, from fd: Int32) throws {
        var offset = 0
        while offset < bytes.count {
            let received = Darwin.read(fd, bytes.baseAddress!.advanced(by: offset), bytes.count - offset)
            guard received > 0 else { throw RPCClientError.socket("本地连接提前关闭。") }
            offset += received
        }
    }
}

func validateProtocolVersion(_ version: UInt32) throws {
    guard version == watchcatProtocolVersion else {
        throw RPCClientError.incompatibleProtocol(version)
    }
}
