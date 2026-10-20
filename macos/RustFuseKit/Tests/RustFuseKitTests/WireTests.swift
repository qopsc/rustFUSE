import Darwin
import Foundation
import XCTest

@testable import RustFuseKit

/// The samples in wire.json are also checked by the daemon's tests
/// (crates/rustfuse-sync/tests/provider_wire.rs), so passing here means
/// the app and the daemon agree.
final class WireTests: XCTestCase {
    let samples: [String: [Any]] = {
        let url = URL(fileURLWithPath: #filePath).deletingLastPathComponent().appendingPathComponent("wire.json")
        let data = try! Data(contentsOf: url)
        return try! JSONSerialization.jsonObject(with: data) as! [String: [Any]]
    }()

    let a1 = "n000000000000000000000000000000a1"

    func testRequestsEncodeLikeTheDaemonExpects() throws {
        let requests: [Request] = [
            .item(id: a1),
            .list(id: rootID, after: "b.txt"),
            .list(id: rootID),
            .anchor,
            .changes(anchor: "00000000000000ff-12"),
            .fetch(id: a1),
            .create(
                parent: rootID, name: "a.txt", kind: .file, source: "/staging/1", mode: 0o644,
                mtimeNs: 1_700_000_000_123_456_789),
            .create(parent: rootID, name: "src", kind: .dir),
            .create(parent: rootID, name: "link", kind: .symlink, target: "a.txt"),
            .modify(id: a1, parent: rootID, name: "b.txt"),
            .modify(id: a1, source: "/staging/2", mode: 0o755, mtimeNs: -5),
            .delete(id: a1, recursive: true),
            .delete(id: "p746172676574"),
            .usage,
            .keep(id: a1, keep: true),
            .keep(id: rootID, keep: false),
            .free(id: "p746172676574"),
        ]
        let expected = samples["requests"]!
        XCTAssertEqual(requests.count, expected.count)
        for (req, want) in zip(requests, expected) {
            let got = try JSONSerialization.jsonObject(with: JSONEncoder().encode(req))
            XCTAssertEqual(got as? NSDictionary, want as? NSDictionary, "\(req)")
        }
    }

    func testResponsesDecode() throws {
        let decoded = try samples["responses"]!.map {
            try JSONDecoder().decode(Response.self, from: JSONSerialization.data(withJSONObject: $0))
        }
        let file = Item(
            id: a1, parent: rootID, name: "a.txt", kind: .file, size: 12, mode: 0o644,
            mtimeNs: 1_700_000_000_123_456_789, target: nil, contentVersion: "1a.2/12/1700000000123456789",
            metaVersion: "1a.2/1b.2/644/root/a.txt", downloaded: false, synced: true, pinned: false)
        let link = Item(
            id: "n000000000000000000000000000000a2", parent: rootID, name: "link", kind: .symlink, size: 5,
            mode: 0o777, mtimeNs: 0, target: "a.txt", contentVersion: "1c.2/5/0",
            metaVersion: "1c.2/1c.2/777/root/link", downloaded: true, synced: true, pinned: true)
        XCTAssertEqual(
            decoded,
            [
                .item(file),
                .list(items: [link], next: "link"),
                .list(items: [], next: nil),
                .anchor("00000000000000ff-13"),
                .changes(updated: [], deleted: ["n000000000000000000000000000000a3"], anchor: "00000000000000ff-14"),
                .path("/volume/a.txt"),
                .done,
                .usage(
                    Usage(
                        files: 120, total: 9_000_000_000, local: 2_500_000_000, git: 1_500_000_000,
                        pinned: 400_000_000, cached: 600_000_000),
                    limit: 21_474_836_480, lazy: true),
                .usage(Usage(files: 0, total: 0, local: 0, git: 0, pinned: 0, cached: 0), limit: nil, lazy: false),
                .error(.anchorExpired, "the anchor is from an earlier run"),
            ])
    }

    func testUnknownErrorCodeIsStillAnError() throws {
        let resp = try JSONDecoder().decode(
            Response.self, from: Data(#"{"result":"error","code":"brand_new","message":"m"}"#.utf8))
        XCTAssertEqual(resp, .error(.io, "m"))
    }

    func testVolumeNames() {
        XCTAssertTrue(Shared.validName("project"))
        XCTAssertTrue(Shared.validName("my-repo_2.x"))
        XCTAssertFalse(Shared.validName(""))
        XCTAssertFalse(Shared.validName(".hidden"))
        XCTAssertFalse(Shared.validName("a/b"))
        XCTAssertFalse(Shared.validName(String(repeating: "a", count: 41)))
    }
}

/// The client against a stand-in daemon on a real socket.
final class ClientTests: XCTestCase {
    var dir: URL!

    override func setUpWithError() throws {
        // Short: socket paths have a 103-byte limit.
        dir = URL(fileURLWithPath: "/tmp/rfk-\(getpid())-\(UUID().uuidString.prefix(8))")
        try FileManager.default.createDirectory(at: dir, withIntermediateDirectories: true)
    }

    override func tearDownWithError() throws {
        try? FileManager.default.removeItem(at: dir)
    }

    func testCallsAndErrors() throws {
        let path = dir.appendingPathComponent("p.sock").path
        let server = try FakeDaemon(path: path) { line in
            let req = try! JSONSerialization.jsonObject(with: line) as! [String: Any]
            switch req["op"] as! String {
            case "anchor": return #"{"result":"anchor","anchor":"e-1"}"#
            case "delete": return #"{"result":"error","code":"not_empty","message":"src"}"#
            default: return #"{"result":"error","code":"bad_request","message":"no"}"#
            }
        }
        defer { server.stop() }
        let client = Client(socketPath: path)
        XCTAssertEqual(try client.anchor(), "e-1")
        // The second call reuses the pooled connection.
        XCTAssertEqual(try client.anchor(), "e-1")
        XCTAssertThrowsError(try client.delete("n1", recursive: false)) {
            XCTAssertEqual($0 as? ProviderError, .daemon(.notEmpty, "src"))
        }
        XCTAssertEqual(server.connections, 1)
    }

    func testNoDaemonIsUnreachable() {
        let client = Client(socketPath: dir.appendingPathComponent("none.sock").path)
        XCTAssertThrowsError(try client.anchor()) {
            guard case .unreachable = $0 as? ProviderError else { return XCTFail("\($0)") }
        }
    }

    func testStaleConnectionIsRetriedForReads() throws {
        let path = dir.appendingPathComponent("p.sock").path
        // Answers one request per connection, then hangs up, like a daemon
        // that restarted.
        let server = try FakeDaemon(path: path, perConnection: 1) { _ in #"{"result":"anchor","anchor":"e-2"}"# }
        defer { server.stop() }
        let client = Client(socketPath: path)
        XCTAssertEqual(try client.anchor(), "e-2")
        XCTAssertEqual(try client.anchor(), "e-2")
        XCTAssertEqual(server.connections, 2)
    }
}

/// Answers each line on a Unix socket with `answer(line)`.
final class FakeDaemon: @unchecked Sendable {
    private let listener: Int32
    private let lock = NSLock()
    private var count = 0
    var connections: Int {
        lock.lock()
        defer { lock.unlock() }
        return count
    }

    init(path: String, perConnection: Int = .max, answer: @escaping @Sendable (Data) -> String) throws {
        unlink(path)
        listener = socket(AF_UNIX, SOCK_STREAM, 0)
        var addr = sockaddr_un()
        addr.sun_family = sa_family_t(AF_UNIX)
        addr.sun_len = UInt8(MemoryLayout<sockaddr_un>.size)
        let bytes = Array(path.utf8)
        withUnsafeMutableBytes(of: &addr.sun_path) { raw in
            raw.copyBytes(from: bytes)
            raw[bytes.count] = 0
        }
        let r = withUnsafePointer(to: &addr) {
            $0.withMemoryRebound(to: sockaddr.self, capacity: 1) {
                bind(listener, $0, socklen_t(MemoryLayout<sockaddr_un>.size))
            }
        }
        guard r == 0, listen(listener, 8) == 0 else { throw POSIXError(.EADDRINUSE) }
        let listener = self.listener
        Thread.detachNewThread { [self] in
            while true {
                let fd = accept(listener, nil, nil)
                if fd < 0 { return }
                lock.lock()
                count += 1
                lock.unlock()
                Thread.detachNewThread {
                    let conn = FakeConn(fd: fd)
                    var served = 0
                    while served < perConnection, let line = conn.readLine() {
                        conn.write(answer(line) + "\n")
                        served += 1
                    }
                    close(fd)
                }
            }
        }
    }

    func stop() {
        shutdown(listener, SHUT_RDWR)
        close(listener)
    }
}

private final class FakeConn {
    let fd: Int32
    var buffer: [UInt8] = []

    init(fd: Int32) {
        self.fd = fd
    }

    func readLine() -> Data? {
        var chunk = [UInt8](repeating: 0, count: 4096)
        while true {
            if let nl = buffer.firstIndex(of: UInt8(ascii: "\n")) {
                let line = Data(buffer[..<nl])
                buffer.removeSubrange(...nl)
                return line
            }
            let n = chunk.withUnsafeMutableBytes { read(fd, $0.baseAddress!, $0.count) }
            if n <= 0 { return nil }
            buffer.append(contentsOf: chunk[..<n])
        }
    }

    func write(_ s: String) {
        let bytes = Array(s.utf8)
        _ = bytes.withUnsafeBytes { Darwin.write(fd, $0.baseAddress!, $0.count) }
    }
}
