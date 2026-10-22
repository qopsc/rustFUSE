import Foundation
import XCTest

@testable import RustFuseKit

final class LibraryTests: XCTestCase {
    let container = URL(fileURLWithPath: "/C")
    let socket = URL(fileURLWithPath: "/C/s/proj.sock")

    func testFirstVersionMigratesToConnections() throws {
        let old = #"[{"name":"proj","hub":"h:7420"},{"name":"docs","hub":"h:7420"},{"name":"x","hub":"tls://o"}]"#
        let lib = Library.migrating(try JSONDecoder().decode([Library.Legacy].self, from: Data(old.utf8)))
        XCTAssertEqual(lib.connections.map(\.hub), ["h:7420", "tls://o"])
        XCTAssertEqual(lib.volumes.map(\.key), ["proj", "docs", "x"])
        XCTAssertEqual(lib.volumes[1].connection, lib.connections[0].id)
        XCTAssertEqual(lib.volumes[2].connection, lib.connections[1].id)
        XCTAssertTrue(lib.volumes.allSatisfy { $0.mode == .finder && $0.cacheSize == defaultCacheSize })
    }

    func testRoundTripAndMissingFieldsDefault() throws {
        var lib = Library()
        let c = Connection(name: "home", hub: "tls://hub", s3: S3Access(bucket: "b", endpoint: "http://nas:9000", accessKey: "AK"))
        lib.connections = [c]
        lib.volumes = [VolumeConfig(key: "proj", name: "proj", connection: c.id, mode: .mount, path: "/Volumes/proj", cacheSize: 5 << 30)]
        lib.settings.onQuit = .keepSyncing
        let back = try JSONDecoder().decode(Library.self, from: JSONEncoder().encode(lib))
        XCTAssertEqual(back, lib)

        let sparse = try JSONDecoder().decode(Library.self, from: Data(#"{"settings":{"onQuit":"keepSyncing"}}"#.utf8))
        XCTAssertEqual(sparse.settings.onQuit, .keepSyncing)
        XCTAssertEqual(sparse.settings.defaultCacheSize, defaultCacheSize)
        XCTAssertEqual(sparse.settings.mountBackend, .fskit)
        XCTAssertTrue(sparse.volumes.isEmpty)
    }

    func testNewKeysAreUnique() {
        var lib = Library()
        XCTAssertEqual(lib.newKey(for: "proj"), "proj")
        lib.volumes = [
            VolumeConfig(key: "proj", name: "proj", connection: UUID(), mode: .finder),
            VolumeConfig(key: "proj-2", name: "proj", connection: UUID(), mode: .folder),
        ]
        XCTAssertEqual(lib.newKey(for: "proj"), "proj-3")
        XCTAssertEqual(lib.newKey(for: "docs"), "docs")
    }

    func testCommandLines() {
        let c = Connection(name: "home", hub: "tls://hub:7420", caFile: "/C/ca/x.pem")
        var settings = AppSettings()
        let finder = VolumeConfig(key: "proj-2", name: "proj", connection: c.id, mode: .finder, cacheSize: 1000)
        let dir = syncedDir(finder, container: container)
        XCTAssertEqual(dir.path, "/C/volumes/proj-2")
        XCTAssertEqual(
            attachArguments(finder, c, dir: dir),
            ["attach", "/C/volumes/proj-2", "--volume", "proj", "--lazy", "--hub", "tls://hub:7420", "--hub-ca", "/C/ca/x.pem"])
        XCTAssertEqual(
            daemonArguments(finder, dir: dir, socket: socket, settings: settings),
            ["sync", "/C/volumes/proj-2", "--provider-socket", "/C/s/proj.sock", "--cache-size", "1000"])

        let folder = VolumeConfig(key: "proj", name: "proj", connection: c.id, mode: .folder, path: "/Users/me/src/proj")
        XCTAssertEqual(syncedDir(folder, container: container).path, "/Users/me/src/proj")
        XCTAssertFalse(attachArguments(folder, c, dir: syncedDir(folder, container: container)).contains("--lazy"))
        XCTAssertEqual(
            daemonArguments(folder, dir: syncedDir(folder, container: container), socket: socket, settings: settings),
            ["sync", "/Users/me/src/proj", "--provider-socket", "/C/s/proj.sock"])

        let mount = VolumeConfig(key: "m", name: "proj", connection: c.id, mode: .mount, path: "/Volumes/proj")
        XCTAssertEqual(
            daemonArguments(mount, dir: syncedDir(mount, container: container), socket: socket, settings: settings),
            [
                "mount", "/C/volumes/m", "/Volumes/proj", "--provider-socket", "/C/s/proj.sock",
                "--cache-size", String(defaultCacheSize), "--fskit",
            ])
        settings.mountBackend = .kernel
        XCTAssertFalse(daemonArguments(mount, dir: dir, socket: socket, settings: settings).contains("--fskit"))
    }

    func testDirectBucketAccess() {
        let c = Connection(
            name: "lan", hub: "nas:7420",
            s3: S3Access(bucket: "code", endpoint: "http://nas:9000", region: "", prefix: "team", accessKey: "AK"))
        XCTAssertEqual(
            volumesArguments(c),
            ["volumes", "--json", "--hub", "nas:7420", "--bucket", "code", "--endpoint", "http://nas:9000", "--prefix", "team"])
        XCTAssertEqual(
            initArguments(name: "new", c, dir: URL(fileURLWithPath: "/Users/me/new")).prefix(4),
            ["init", "/Users/me/new", "--volume", "new"])
        XCTAssertEqual(
            initArguments(name: "new", c, dir: URL(fileURLWithPath: "/Users/me/new"), encrypt: true).prefix(5),
            ["init", "/Users/me/new", "--volume", "new", "--encrypt"])
        XCTAssertEqual(toolEnvironment(base: [:], token: nil, s3Secret: nil, c, passphrase: "pw")["RUSTFUSE_PASSPHRASE"], "pw")
        XCTAssertNil(toolEnvironment(base: [:], token: nil, s3Secret: nil, c, passphrase: "")["RUSTFUSE_PASSPHRASE"])
        let env = toolEnvironment(base: ["PATH": "/bin"], token: "t", s3Secret: "S", c)
        XCTAssertEqual(env["RUSTFUSE_HUB_TOKEN"], "t")
        XCTAssertEqual(env["RUSTFUSE_ACCESS_KEY"], "AK")
        XCTAssertEqual(env["RUSTFUSE_SECRET_KEY"], "S")
        XCTAssertEqual(env["RUST_LOG"], "info")
        let hubOnly = toolEnvironment(base: [:], token: nil, s3Secret: nil, Connection(name: "h", hub: "h"))
        XCTAssertNil(hubOnly["RUSTFUSE_ACCESS_KEY"])
        XCTAssertNil(hubOnly["RUSTFUSE_HUB_TOKEN"])
    }

    /// library.json is also read by the terminal UI's tests
    /// (crates/rustfuse-tui/src/library.rs), so both keep the same file.
    func testSharedSampleRoundTrips() throws {
        let url = URL(fileURLWithPath: #filePath).deletingLastPathComponent().appendingPathComponent("library.json")
        let data = try Data(contentsOf: url)
        let lib = try JSONDecoder().decode(Library.self, from: data)
        XCTAssertEqual(lib.volumes.map(\.mode), [.finder, .folder, .mount])
        XCTAssertEqual(lib.settings.onQuit, .keepSyncing)
        XCTAssertEqual(lib.connections[1].s3?.prefix, "team/")
        let back = try JSONSerialization.jsonObject(with: JSONEncoder().encode(lib)) as! NSDictionary
        XCTAssertEqual(back, try JSONSerialization.jsonObject(with: data) as! NSDictionary)
    }
}
