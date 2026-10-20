import Darwin
import Foundation

public enum ProviderError: Error, Equatable {
    /// The daemon answered with an error.
    case daemon(ErrorCode, String)
    /// The daemon is not running or went away.
    case unreachable(String)
    /// The daemon answered something this version does not understand.
    case badAnswer(String)
}

/// Talks to `rustfuse sync --provider-socket`. Thread-safe: each call takes
/// an idle connection or opens one, so a long fetch does not hold up
/// listings (the daemon answers connections concurrently).
public final class Client: @unchecked Sendable {
    public let socketPath: String
    private let lock = NSLock()
    private var idle: [SocketConnection] = []
    private let encoder: JSONEncoder = {
        let e = JSONEncoder()
        e.outputFormatting = .withoutEscapingSlashes
        return e
    }()
    private let decoder = JSONDecoder()

    public init(socketPath: String) {
        self.socketPath = socketPath
    }

    deinit {
        close()
    }

    /// Closes idle connections.
    public func close() {
        lock.lock()
        let conns = idle
        idle = []
        lock.unlock()
        conns.forEach { $0.close() }
    }

    /// Sends `request` and returns the answer; an error answer is returned
    /// as is.
    public func send(_ request: Request) throws -> Response {
        var line = try encoder.encode(request)
        line.append(UInt8(ascii: "\n"))
        // A pooled connection may have gone stale (the daemon restarted):
        // a read-only request is retried once on a new one.
        var retried = false
        while true {
            let (conn, pooled) = try take()
            let answer: Data
            do {
                try conn.write(line)
                answer = try conn.readLine()
            } catch let e as ProviderError {
                conn.close()
                if case .unreachable = e, pooled, !retried, request.isReadOnly {
                    retried = true
                    continue
                }
                throw e
            }
            give(conn)
            do {
                return try decoder.decode(Response.self, from: answer)
            } catch {
                throw ProviderError.badAnswer("\(error)")
            }
        }
    }

    /// Sends `request`, turning an error answer into a thrown
    /// `ProviderError.daemon`.
    public func call(_ request: Request) throws -> Response {
        let resp = try send(request)
        if case .error(let code, let message) = resp {
            throw ProviderError.daemon(code, message)
        }
        return resp
    }

    public func item(_ id: String) throws -> Item {
        guard case .item(let item) = try call(.item(id: id)) else { throw unexpected("item") }
        return item
    }

    public func list(_ id: String, after: String? = nil) throws -> (items: [Item], next: String?) {
        guard case .list(let items, let next) = try call(.list(id: id, after: after)) else {
            throw unexpected("list")
        }
        return (items, next)
    }

    public func anchor() throws -> String {
        guard case .anchor(let a) = try call(.anchor) else { throw unexpected("anchor") }
        return a
    }

    public func changes(since anchor: String) throws -> (updated: [Item], deleted: [String], anchor: String) {
        guard case .changes(let updated, let deleted, let next) = try call(.changes(anchor: anchor)) else {
            throw unexpected("changes")
        }
        return (updated, deleted, next)
    }

    /// The path of the item's content in the volume, fetched first if it
    /// was not on this machine.
    public func fetch(_ id: String) throws -> String {
        guard case .path(let p) = try call(.fetch(id: id)) else { throw unexpected("fetch") }
        return p
    }

    /// Sends a create or modify and returns the item it left.
    public func change(_ request: Request) throws -> Item {
        guard case .item(let item) = try call(request) else { throw unexpected("an item") }
        return item
    }

    public func delete(_ id: String, recursive: Bool) throws {
        guard case .done = try call(.delete(id: id, recursive: recursive)) else { throw unexpected("done") }
    }

    public func usage() throws -> (usage: Usage, limit: UInt64?, lazy: Bool) {
        guard case .usage(let u, let limit, let lazy) = try call(.usage) else { throw unexpected("usage") }
        return (u, limit, lazy)
    }

    /// Keeps an item downloaded (or stops keeping it).
    public func keep(_ id: String, _ keep: Bool) throws {
        guard case .done = try call(.keep(id: id, keep: keep)) else { throw unexpected("done") }
    }

    /// Frees the space an item's downloaded content takes.
    public func free(_ id: String) throws {
        guard case .done = try call(.free(id: id)) else { throw unexpected("done") }
    }

    private func unexpected(_ wanted: String) -> ProviderError {
        .badAnswer("expected \(wanted)")
    }

    private func take() throws -> (SocketConnection, Bool) {
        lock.lock()
        let conn = idle.popLast()
        lock.unlock()
        if let conn {
            return (conn, true)
        }
        return (try SocketConnection(path: socketPath), false)
    }

    private func give(_ conn: SocketConnection) {
        lock.lock()
        if idle.count < 8 {
            idle.append(conn)
            lock.unlock()
        } else {
            lock.unlock()
            conn.close()
        }
    }
}

/// One socket connection, used by one call at a time.
final class SocketConnection {
    private var fd: Int32
    private var buffer: [UInt8] = []

    init(path: String) throws {
        var addr = sockaddr_un()
        addr.sun_family = sa_family_t(AF_UNIX)
        let bytes = Array(path.utf8)
        let capacity = MemoryLayout.size(ofValue: addr.sun_path)
        guard bytes.count < capacity else {
            throw ProviderError.unreachable("socket path longer than \(capacity - 1) bytes: \(path)")
        }
        withUnsafeMutableBytes(of: &addr.sun_path) { raw in
            raw.copyBytes(from: bytes)
            raw[bytes.count] = 0
        }
        addr.sun_len = UInt8(MemoryLayout<sockaddr_un>.size)
        fd = socket(AF_UNIX, SOCK_STREAM, 0)
        guard fd >= 0 else { throw ProviderError.unreachable(SocketConnection.errno("socket")) }
        var on: Int32 = 1
        setsockopt(fd, SOL_SOCKET, SO_NOSIGPIPE, &on, socklen_t(MemoryLayout<Int32>.size))
        let r = withUnsafePointer(to: &addr) {
            $0.withMemoryRebound(to: sockaddr.self, capacity: 1) {
                connect(fd, $0, socklen_t(MemoryLayout<sockaddr_un>.size))
            }
        }
        guard r == 0 else {
            let message = SocketConnection.errno("connecting to \(path)")
            Darwin.close(fd)
            fd = -1
            throw ProviderError.unreachable(message)
        }
    }

    deinit {
        close()
    }

    func close() {
        if fd >= 0 {
            Darwin.close(fd)
            fd = -1
        }
    }

    func write(_ data: Data) throws {
        try data.withUnsafeBytes { (raw: UnsafeRawBufferPointer) in
            var off = 0
            while off < raw.count {
                let n = Darwin.write(fd, raw.baseAddress! + off, raw.count - off)
                if n < 0 {
                    if Darwin.errno == EINTR { continue }
                    throw ProviderError.unreachable(SocketConnection.errno("writing"))
                }
                off += n
            }
        }
    }

    /// The next line, without its newline.
    func readLine() throws -> Data {
        var chunk = [UInt8](repeating: 0, count: 64 * 1024)
        while true {
            if let nl = buffer.firstIndex(of: UInt8(ascii: "\n")) {
                let line = Data(buffer[..<nl])
                buffer.removeSubrange(...nl)
                return line
            }
            let n = chunk.withUnsafeMutableBytes { Darwin.read(fd, $0.baseAddress!, $0.count) }
            if n < 0 {
                if Darwin.errno == EINTR { continue }
                throw ProviderError.unreachable(SocketConnection.errno("reading"))
            }
            if n == 0 {
                throw ProviderError.unreachable("the daemon closed the connection")
            }
            buffer.append(contentsOf: chunk[..<n])
        }
    }

    private static func errno(_ what: String) -> String {
        "\(what): \(String(cString: strerror(Darwin.errno)))"
    }
}
